//! The Gunda background download daemon.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gunda_capture::{
    Candidate, CandidateId, CaptureHandle, CaptureServer, PairingToken, load_or_create_token,
    token_path,
};
use gunda_core::application::{
    DownloadCancellation, DownloadExecutor, DownloadManager, DownloadManagerError, ExecutionOutput,
    ExecutionReport,
};
use gunda_core::download::{
    DownloadDestination, DownloadFailure, DownloadId, DownloadJob, DownloadOrigin, DownloadState,
    ExecutionSelection, FailureKind, FileConflictPolicy, NewDownload, RequestContext,
    ResourceDescriptor, ResourceKind,
};
use gunda_http::{HttpClient, HttpExecutor};
use gunda_ipc::{
    CaptureStatus as IpcCaptureStatus, CaptureTrackChoice, CaptureTrackView, CaptureTracks,
    CaptureView, ClientMessage, Command, Connection, JobView, NativePath, PROTOCOL_VERSION,
    ServerMessage, Snapshot,
};
use gunda_storage::{RuntimeLease, SqliteDownloadRepository};
use gunda_stream::{PresentationTrack, StreamExecutor, TrackKind, TrackSize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;
use url::Url;

const MAX_AUTOMATIC_ATTEMPTS: u8 = 3;
const MAX_CONCURRENT_DOWNLOADS: usize = 3;
const GREETING_DEADLINE: Duration = Duration::from_secs(10);
const CAPTURE_PORT: u16 = 48_123;

/// Failure while starting or running the daemon.
#[derive(Debug)]
pub enum DaemonError {
    Io(io::ErrorKind),
    Config(&'static str),
    Runtime,
    Ipc(gunda_ipc::IpcError),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(kind) => write!(f, "daemon I/O failed: {kind:?}"),
            Self::Config(message) => f.write_str(message),
            Self::Runtime => f.write_str("daemon runtime could not start"),
            Self::Ipc(error) => write!(f, "daemon IPC failed: {error}"),
        }
    }
}

impl Error for DaemonError {}

impl From<io::Error> for DaemonError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

impl From<gunda_ipc::IpcError> for DaemonError {
    fn from(error: gunda_ipc::IpcError) -> Self {
        Self::Ipc(error)
    }
}

struct Engines {
    http: HttpExecutor,
    stream: StreamExecutor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobEngine {
    Http,
    Stream,
}

enum WorkerEvent {
    Changed,
    Notice(String),
    ClearNotice,
    Inspected {
        sequence: u64,
        id: CandidateId,
        tracks: CaptureTracks,
    },
    Finished {
        id: DownloadId,
        result: Result<ExecutionReport, DownloadManagerError>,
    },
}

struct CommandRequest {
    command: Command,
    response: oneshot::Sender<Result<(), String>>,
}

struct CreateInput {
    url: String,
    directory: NativePath,
    preferred_filename: Option<String>,
    selection: Option<String>,
    page_url: Option<String>,
    page_title: Option<String>,
    engine: Option<String>,
}

struct Daemon {
    manager: Arc<DownloadManager<SqliteDownloadRepository>>,
    engines: Arc<Engines>,
    active_jobs: BTreeMap<DownloadId, DownloadCancellation>,
    pending_engines: BTreeMap<DownloadId, JobEngine>,
    pending_outputs: BTreeMap<DownloadId, ExecutionOutput>,
    worker_tx: mpsc::UnboundedSender<WorkerEvent>,
    capture: Option<CaptureHandle>,
    capture_problem: Option<String>,
    capture_tracks: Option<CaptureTracks>,
    inspection_sequence: u64,
    notice: Option<String>,
}

impl Daemon {
    async fn handle_command(&mut self, command: Command) -> Result<(), String> {
        match command {
            Command::Create {
                url,
                directory,
                preferred_filename,
                selection,
                page_url,
                page_title,
                engine,
            } => {
                self.create(CreateInput {
                    url,
                    directory,
                    preferred_filename,
                    selection,
                    page_url,
                    page_title,
                    engine,
                })
                .await
            }
            Command::Pause { id } => self.pause(download_id(id)?).await,
            Command::Resume { id } => self.resume(download_id(id)?).await,
            Command::Retry { id } => self.retry(download_id(id)?).await,
            Command::Restart { id } => self.restart(download_id(id)?).await,
            Command::Cancel { id } => self.cancel(download_id(id)?).await,
            Command::SaveStatus { id } => self.save_status(download_id(id)?).await,
            Command::InspectCapture { id } => self.inspect_capture(id),
            Command::DownloadCapture {
                id,
                directory,
                track,
            } => self.download_capture(id, directory, track).await,
            Command::DismissCapture { id } => self.dismiss_capture(id).await,
            Command::ClearCaptures => self.clear_captures().await,
        }
    }

    async fn create(&mut self, input: CreateInput) -> Result<(), String> {
        let CreateInput {
            url,
            directory,
            preferred_filename,
            selection,
            page_url,
            page_title,
            engine,
        } = input;
        let url = validate_url(&url)?;
        let directory = directory
            .to_path()
            .map_err(|_| "Invalid destination folder.")?;
        let selection = selection
            .map(ExecutionSelection::new)
            .transpose()
            .map_err(|_| "Invalid stream selection.")?;
        let origin = match page_url {
            Some(page_url) => DownloadOrigin::Browser {
                page_url: validate_url(&page_url)?,
                page_title,
            },
            None => DownloadOrigin::Desktop,
        };
        let job_engine = match engine.as_deref() {
            Some("stream") => JobEngine::Stream,
            Some("http") => JobEngine::Http,
            None => engine_for(&url, None),
            Some(_) => return Err("Unknown download engine.".into()),
        };
        let mut request = RequestContext::new(url, Vec::new());
        if let Some(selection) = selection {
            request = request.with_selection(selection);
        }

        let id = self
            .manager
            .create(NewDownload::new(
                request,
                DownloadDestination::new(directory, preferred_filename, FileConflictPolicy::Rename),
                origin,
            ))
            .await
            .map_err(|_| "Could not save the new download.")?
            .download_id();

        self.pending_engines.insert(id, job_engine);
        Ok(())
    }

    async fn pause(&mut self, id: DownloadId) -> Result<(), String> {
        if let Some(control) = self.active_jobs.get(&id).cloned() {
            return if control.request_pause() {
                Ok(())
            } else {
                Err("This download cannot be paused safely.".into())
            };
        }

        self.manager
            .pause(id)
            .await
            .map(|_| ())
            .map_err(|_| "This download cannot be paused.".into())
    }

    async fn resume(&mut self, id: DownloadId) -> Result<(), String> {
        if self.active_jobs.contains_key(&id) {
            return Err("This download is already running.".into());
        }

        let engine = self.engine_of(id);
        let available = match self.manager.job(id) {
            Some(job) => match engine {
                JobEngine::Http => self.engines.http.can_resume(&job).await,
                JobEngine::Stream => self.engines.stream.can_resume(&job).await,
            },
            None => return Err("This download does not exist.".into()),
        }
        .map_err(|_| "Could not check saved progress.".to_string())?;

        if !available {
            return Err(
                "Saved progress is unavailable. Use Restart to download again from zero.".into(),
            );
        }

        self.manager
            .resume(id)
            .await
            .map(|_| ())
            .map_err(|_| "This download cannot be resumed.".to_string())?;
        self.pending_engines.insert(id, engine);
        Ok(())
    }

    async fn retry(&mut self, id: DownloadId) -> Result<(), String> {
        if self.active_jobs.contains_key(&id) {
            return Err("This download is already running.".into());
        }

        let engine = self.engine_of(id);
        self.manager
            .retry(id)
            .await
            .map(|_| ())
            .map_err(|_| "This download cannot be retried.".to_string())?;
        self.pending_engines.insert(id, engine);
        Ok(())
    }

    async fn restart(&mut self, id: DownloadId) -> Result<(), String> {
        if self.active_jobs.contains_key(&id) {
            return Err("This download is still running.".into());
        }

        let engine = self.engine_of(id);
        let directory = self
            .manager
            .job(id)
            .filter(|job| job.state().can_restart())
            .map(|job| job.destination().directory().to_path_buf())
            .ok_or_else(|| "This download cannot be restarted.".to_string())?;

        let reset = match engine {
            JobEngine::Http => self
                .engines
                .http
                .reset_for_restart(id, &directory)
                .await
                .is_err(),
            JobEngine::Stream => self
                .engines
                .stream
                .reset_for_restart(id, &directory)
                .await
                .is_err(),
        };
        if reset {
            return Err("The previous download data could not be cleared safely.".into());
        }

        self.manager
            .restart(id)
            .await
            .map(|_| ())
            .map_err(|_| "This download could not be restarted.".to_string())?;
        self.pending_engines.insert(id, engine);
        Ok(())
    }

    async fn cancel(&mut self, id: DownloadId) -> Result<(), String> {
        if let Some(control) = self.active_jobs.get(&id).cloned() {
            control.request();
            return Ok(());
        }

        self.manager
            .cancel(id)
            .await
            .map(|_| ())
            .map_err(|_| "This download cannot be cancelled.".into())
    }

    async fn save_status(&mut self, id: DownloadId) -> Result<(), String> {
        let output = self
            .pending_outputs
            .get(&id)
            .ok_or_else(|| "There is no saved file status to update.".to_string())?;

        self.manager
            .confirm_published_output(id, output)
            .await
            .map_err(|_| "The file is saved, but its status could not be updated.".to_string())?;
        self.pending_outputs.remove(&id);
        Ok(())
    }

    fn inspect_capture(&mut self, id: CandidateId) -> Result<(), String> {
        let captured = self.captured(id)?;
        self.inspection_sequence = self.inspection_sequence.saturating_add(1);
        let sequence = self.inspection_sequence;
        self.capture_tracks = None;
        let engines = Arc::clone(&self.engines);
        let updates = self.worker_tx.clone();

        tokio::spawn(async move {
            let request = RequestContext::new(captured.url, Vec::new());
            let tracks = match engines.stream.inspect(&request).await {
                Ok(presentation) => presentation
                    .tracks()
                    .iter()
                    .map(capture_track_view)
                    .collect(),
                Err(_) => {
                    let _ = updates.send(WorkerEvent::Inspected {
                        sequence,
                        id,
                        tracks: CaptureTracks {
                            id,
                            tracks: Vec::new(),
                            problem: Some(
                                "The qualities of this presentation could not be read.".into(),
                            ),
                        },
                    });
                    return;
                }
            };

            let _ = updates.send(WorkerEvent::Inspected {
                sequence,
                id,
                tracks: CaptureTracks {
                    id,
                    tracks,
                    problem: None,
                },
            });
        });
        Ok(())
    }

    async fn download_capture(
        &mut self,
        id: CandidateId,
        directory: NativePath,
        track: Option<CaptureTrackChoice>,
    ) -> Result<(), String> {
        let captured = self.captured(id)?;
        let directory = directory
            .to_path()
            .map_err(|_| "Invalid destination folder.")?;
        let (url, filename, selection) = match track {
            Some(track) => {
                let url = validate_url(&track.url)?;
                let selection = track
                    .selection
                    .map(ExecutionSelection::new)
                    .transpose()
                    .map_err(|_| "Invalid stream selection.")?;
                (url, track.filename, selection)
            }
            None => (captured.url, None, None),
        };
        let mut request = RequestContext::new(url, Vec::new());
        if let Some(selection) = selection {
            request = request.with_selection(selection);
        }

        let id = self
            .manager
            .create(NewDownload::new(
                request,
                DownloadDestination::new(directory, filename, FileConflictPolicy::Rename),
                captured.origin,
            ))
            .await
            .map_err(|_| "Could not save the new download.")?
            .download_id();

        self.pending_engines.insert(id, captured.engine);
        self.capture_tracks = None;
        self.inspection_sequence = self.inspection_sequence.saturating_add(1);
        Ok(())
    }

    async fn dismiss_capture(&mut self, id: CandidateId) -> Result<(), String> {
        self.capture_handle()?
            .remove(id)
            .await
            .map_err(|_| "The capture could not be dismissed.".to_string())?;
        if self
            .capture_tracks
            .as_ref()
            .is_some_and(|tracks| tracks.id == id)
        {
            self.capture_tracks = None;
        }
        self.inspection_sequence = self.inspection_sequence.saturating_add(1);
        Ok(())
    }

    async fn clear_captures(&mut self) -> Result<(), String> {
        self.capture_handle()?
            .clear()
            .await
            .map_err(|_| "The capture list could not be cleared.".to_string())?;
        self.capture_tracks = None;
        self.inspection_sequence = self.inspection_sequence.saturating_add(1);
        Ok(())
    }

    fn capture_handle(&self) -> Result<&CaptureHandle, String> {
        self.capture
            .as_ref()
            .ok_or_else(|| "Browser capture is not available.".to_string())
    }

    fn captured(&self, id: CandidateId) -> Result<Captured, String> {
        let candidate = self
            .capture_handle()?
            .view()
            .get(id)
            .cloned()
            .ok_or_else(|| "That capture is no longer in the list.".to_string())?;
        let engine = capture_engine(&candidate)?;

        Ok(Captured {
            url: candidate.url().clone(),
            origin: candidate.origin(),
            engine,
        })
    }

    fn engine_of(&self, id: DownloadId) -> JobEngine {
        self.manager
            .job(id)
            .map(|job| {
                engine_for(
                    job.request().url(),
                    job.resource().map(ResourceDescriptor::kind),
                )
            })
            .unwrap_or(JobEngine::Http)
    }

    fn schedule_jobs(&mut self) {
        while self.active_jobs.len() < MAX_CONCURRENT_DOWNLOADS {
            let Some(job) = self.manager.jobs().into_iter().find(|job| {
                job.state() == DownloadState::Queued && !self.active_jobs.contains_key(&job.id())
            }) else {
                break;
            };
            let id = job.id();
            let engine = self.pending_engines.remove(&id).unwrap_or_else(|| {
                engine_for(
                    job.request().url(),
                    job.resource().map(ResourceDescriptor::kind),
                )
            });
            let cancellation = DownloadCancellation::new();
            self.active_jobs.insert(id, cancellation.clone());

            let manager = Arc::clone(&self.manager);
            let engines = Arc::clone(&self.engines);
            let updates = self.worker_tx.clone();
            tokio::spawn(async move {
                let result = match engine {
                    JobEngine::Http => {
                        execute_job(&manager, &engines.http, id, cancellation, &updates).await
                    }
                    JobEngine::Stream => {
                        execute_job(&manager, &engines.stream, id, cancellation, &updates).await
                    }
                };
                let _ = updates.send(WorkerEvent::Finished { id, result });
            });
        }
    }

    async fn handle_worker_event(&mut self, event: Option<WorkerEvent>) {
        let Some(event) = event else {
            return;
        };

        match event {
            WorkerEvent::Notice(message) => self.notice = Some(message),
            WorkerEvent::ClearNotice => self.notice = None,
            WorkerEvent::Changed => {}
            WorkerEvent::Inspected {
                sequence,
                id,
                tracks,
            } => {
                let candidate_exists = self
                    .capture
                    .as_ref()
                    .is_some_and(|capture| capture.view().get(id).is_some());
                if sequence == self.inspection_sequence && candidate_exists {
                    self.capture_tracks = Some(tracks);
                }
            }
            WorkerEvent::Finished { id, result } => {
                self.active_jobs.remove(&id);
                match result {
                    Err(DownloadManagerError::CompletionNotPersisted {
                        destination,
                        written_bytes,
                        cleanup_pending,
                        ..
                    }) => {
                        self.pending_outputs.insert(
                            id,
                            ExecutionOutput {
                                destination,
                                written_bytes,
                                cleanup_pending,
                            },
                        );
                        self.notice = Some("The file was saved. Use Save status to update its download record without downloading again.".into());
                    }
                    Err(_) => {
                        self.notice =
                            Some("The download could not be completed. Check its details.".into())
                    }
                    Ok(_) => {}
                }
                self.schedule_jobs();
            }
        }
    }

    async fn snapshot(&self) -> Result<Snapshot, gunda_ipc::IpcError> {
        let mut jobs = Vec::new();

        for job in self.manager.jobs() {
            jobs.push(self.job_view(&job).await?);
        }

        Ok(Snapshot {
            jobs,
            active: self.active_jobs.keys().map(|id| id.value()).collect(),
            captures: self
                .capture
                .as_ref()
                .map(|capture| {
                    capture
                        .view()
                        .candidates()
                        .iter()
                        .map(capture_view)
                        .collect()
                })
                .unwrap_or_default(),
            capture_tracks: self.capture_tracks.clone(),
            capture: self.capture.as_ref().map(|capture| IpcCaptureStatus {
                endpoint: capture.endpoint(),
                token: capture.token().as_str().to_owned(),
            }),
            capture_problem: self.capture_problem.clone(),
            notice: self.notice.clone(),
        })
    }

    async fn job_view(&self, job: &DownloadJob) -> Result<JobView, gunda_ipc::IpcError> {
        let id = job.id();
        let active = self.active_jobs.get(&id);
        let resume_available = if active.is_some() {
            false
        } else {
            match self.engine_of(id) {
                JobEngine::Http => self.engines.http.can_resume(job).await,
                JobEngine::Stream => self.engines.stream.can_resume(job).await,
            }
            .unwrap_or(false)
        };
        let failure = job.last_failure();

        Ok(JobView {
            id: id.value(),
            name: job_name(job),
            state: state_name(job.state()).to_owned(),
            written: job.progress().downloaded_bytes(),
            total: job.progress().total_bytes(),
            source: job.request().url().origin().ascii_serialization(),
            origin: match job.origin() {
                DownloadOrigin::Desktop => "Desktop",
                DownloadOrigin::Browser { .. } => "Browser",
                DownloadOrigin::Cli => "CLI",
            }
            .into(),
            destination: NativePath::from_path(job.destination().directory())?,
            output: job
                .resolved_destination()
                .filter(|_| job.state() == DownloadState::Completed)
                .map(|destination| NativePath::from_path(destination.final_path()))
                .transpose()?,
            failure_kind: failure.map(|failure| failure_kind_name(failure.kind()).to_owned()),
            failure_message: failure.map(|failure| failure.message().to_owned()),
            retryable: failure.is_some_and(DownloadFailure::is_retryable),
            resume_available,
            pause_supported: active.is_some_and(DownloadCancellation::pause_supported),
            completion_pending: self.pending_outputs.contains_key(&id),
        })
    }
}

/// Runs the daemon until the process receives an interrupt.
pub async fn run(directory: PathBuf) -> Result<(), DaemonError> {
    std::fs::create_dir_all(&directory)?;
    let _lease = RuntimeLease::acquire(directory.join("gunda.runtime.lock"))
        .map_err(|_| DaemonError::Config("Another Gunda process is using this data directory."))?;
    let repository = SqliteDownloadRepository::open(directory.join("gunda.sqlite3"))
        .await
        .map_err(|_| DaemonError::Config("Could not open saved downloads."))?;
    let http_store = repository.http_resume_store();
    let stream_store = repository.stream_resume_store();
    let manager = Arc::new(
        DownloadManager::start(repository)
            .await
            .map_err(|_| DaemonError::Config("Could not load saved downloads."))?,
    );
    let engines = Arc::new(Engines {
        http: HttpExecutor::with_resume_store(
            HttpClient::new().map_err(|_| DaemonError::Runtime)?,
            http_store,
        ),
        stream: StreamExecutor::with_resume_store(
            HttpClient::new().map_err(|_| DaemonError::Runtime)?,
            stream_store,
        ),
    });
    let (capture, capture_problem) = start_capture(&directory).await;
    let token = load_or_create_token(&directory.join("gundad-token"))
        .map_err(|_| DaemonError::Config("Could not load the daemon token."))?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    gunda_ipc::write_endpoint(&directory, &format!("127.0.0.1:{}", address.port()))?;

    let (snapshot_tx, snapshot_rx) = watch::channel(Snapshot::default());
    let (command_tx, mut command_rx) = mpsc::channel(32);
    let (worker_tx, mut worker_rx) = mpsc::unbounded_channel();
    tokio::spawn(accept_clients(listener, token, command_tx, snapshot_rx));

    let mut daemon = Daemon {
        manager,
        engines,
        active_jobs: BTreeMap::new(),
        pending_engines: BTreeMap::new(),
        pending_outputs: BTreeMap::new(),
        worker_tx,
        capture,
        capture_problem,
        capture_tracks: None,
        inspection_sequence: 0,
        notice: None,
    };
    publish_snapshot(&daemon, &snapshot_tx).await?;

    loop {
        tokio::select! {
            request = command_rx.recv() => {
                let Some(request) = request else {
                    break;
                };
                let result = daemon.handle_command(request.command).await;
                let _ = request.response.send(result);
                daemon.schedule_jobs();
                publish_snapshot(&daemon, &snapshot_tx).await?;
            }
            event = worker_rx.recv() => {
                daemon.handle_worker_event(event).await;
                publish_snapshot(&daemon, &snapshot_tx).await?;
            }
            changed = capture_changed(&mut daemon.capture) => {
                daemon.handle_capture_change(changed);
                publish_snapshot(&daemon, &snapshot_tx).await?;
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    Ok(())
}

async fn start_capture(directory: &Path) -> (Option<CaptureHandle>, Option<String>) {
    let token = match load_or_create_token(&token_path(directory)) {
        Ok(token) => token,
        Err(_) => {
            return (
                None,
                Some(
                    "The browser capture channel could not be set up. Check the application data folder."
                        .into(),
                ),
            );
        }
    };

    let preferred = CaptureServer::bind_to(
        SocketAddr::from((Ipv4Addr::LOCALHOST, CAPTURE_PORT)),
        token.clone(),
    )
    .await;
    let bound = match preferred {
        Ok(bound) => Ok(bound),
        Err(_) => CaptureServer::bind(token).await,
    };

    match bound {
        Ok((server, handle)) => {
            tokio::spawn(server.run());
            (Some(handle), None)
        }
        Err(_) => (
            None,
            Some("The browser capture channel could not be started.".into()),
        ),
    }
}

async fn capture_changed(capture: &mut Option<CaptureHandle>) -> bool {
    match capture {
        Some(handle) => handle.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

impl Daemon {
    fn handle_capture_change(&mut self, running: bool) {
        if running {
            return;
        }

        self.capture = None;
        self.capture_problem = Some("The browser capture channel stopped.".into());
        self.capture_tracks = None;
        self.inspection_sequence = self.inspection_sequence.saturating_add(1);
    }
}

async fn publish_snapshot(
    daemon: &Daemon,
    sender: &watch::Sender<Snapshot>,
) -> Result<(), gunda_ipc::IpcError> {
    sender.send_replace(daemon.snapshot().await?);
    Ok(())
}

async fn accept_clients(
    listener: TcpListener,
    token: PairingToken,
    commands: mpsc::Sender<CommandRequest>,
    snapshots: watch::Receiver<Snapshot>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let token = token.clone();
        let commands = commands.clone();
        let snapshots = snapshots.clone();
        tokio::spawn(async move {
            let _ = serve_client(stream, token, commands, snapshots).await;
        });
    }
}

async fn serve_client(
    stream: TcpStream,
    token: PairingToken,
    commands: mpsc::Sender<CommandRequest>,
    mut snapshots: watch::Receiver<Snapshot>,
) -> Result<(), gunda_ipc::IpcError> {
    let mut connection = Connection::from_stream(stream);
    let hello = timeout(GREETING_DEADLINE, connection.receive::<ClientMessage>())
        .await
        .map_err(|_| gunda_ipc::IpcError::InvalidMessage)??;

    match hello {
        ClientMessage::Hello {
            version,
            token: presented,
        } if version == PROTOCOL_VERSION && token.matches(&presented) => {}
        _ => {
            let _ = connection
                .send(&ServerMessage::Error {
                    message: "Unauthorized daemon client.".into(),
                })
                .await;
            return Err(gunda_ipc::IpcError::Unauthorized);
        }
    }

    let (mut reader, mut writer) = connection.into_parts();
    let snapshot = snapshots.borrow().clone();
    writer.send(&ServerMessage::Snapshot { snapshot }).await?;

    loop {
        tokio::select! {
            message = reader.receive::<ClientMessage>() => {
                match message? {
                    ClientMessage::Command { command } => {
                        let (response, result) = oneshot::channel();
                        if commands.send(CommandRequest { command, response }).await.is_err() {
                            return Ok(());
                        }
                        if let Ok(Err(message)) = result.await {
                            writer.send(&ServerMessage::Error { message }).await?;
                        }
                    }
                    ClientMessage::Hello { .. } => {
                        writer
                            .send(&ServerMessage::Error {
                                message: "The daemon connection was already authenticated.".into(),
                            })
                            .await?;
                    }
                }
            }
            changed = snapshots.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let snapshot = snapshots.borrow().clone();
                writer.send(&ServerMessage::Snapshot { snapshot }).await?;
            }
        }
    }
}

async fn execute_job<E: DownloadExecutor>(
    manager: &DownloadManager<SqliteDownloadRepository>,
    executor: &E,
    id: DownloadId,
    cancellation: DownloadCancellation,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) -> Result<ExecutionReport, DownloadManagerError> {
    let control = cancellation.clone();
    let mut attempts = 0;

    loop {
        attempts += 1;
        let result = manager
            .execute_controlled_with_updates(id, executor, cancellation.clone(), |_, _| {
                let _ = updates.send(WorkerEvent::Changed);
            })
            .await;

        let retryable = matches!(
            &result,
            Ok(report)
                if matches!(
                    &report.event,
                    gunda_core::application::DownloadEvent::Failed { failure, .. }
                        if failure.is_retryable()
                )
        );
        if !retryable || attempts >= MAX_AUTOMATIC_ATTEMPTS {
            return result;
        }

        manager.retry(id).await?;
        let delay_seconds = 1_u64 << (attempts - 1);
        let _ = updates.send(WorkerEvent::Notice(format!(
            "Connection problem. Trying again in {delay_seconds} seconds…"
        )));

        tokio::select! {
            _ = control.stopped() => {}
            _ = tokio::time::sleep(Duration::from_secs(delay_seconds)) => {}
        }
        let _ = updates.send(WorkerEvent::ClearNotice);
    }
}

struct Captured {
    url: Url,
    origin: DownloadOrigin,
    engine: JobEngine,
}

fn capture_engine(candidate: &Candidate) -> Result<JobEngine, String> {
    match candidate.kind() {
        ResourceKind::File => Ok(JobEngine::Http),
        ResourceKind::Hls | ResourceKind::Dash => {
            if path_names_stream(candidate.url()) {
                Ok(JobEngine::Stream)
            } else {
                Err("This capture is a segmented stream whose URL does not name its format.".into())
            }
        }
        ResourceKind::Unknown => Err("This capture is not something Gunda can download.".into()),
    }
}

fn capture_view(candidate: &Candidate) -> CaptureView {
    CaptureView {
        id: candidate.id(),
        url: candidate.url().as_str().to_owned(),
        kind: capture_kind_name(candidate.kind()).to_owned(),
        content_type: candidate.content_type().map(str::to_owned),
        page_title: candidate.page_title().map(str::to_owned),
        seen: candidate.seen(),
        segmented: matches!(candidate.kind(), ResourceKind::Hls | ResourceKind::Dash),
    }
}

fn capture_track_view(track: &PresentationTrack) -> CaptureTrackView {
    CaptureTrackView {
        label: track.label().to_owned(),
        details: format!(
            "{} | {}",
            track_kind_name(track.kind()),
            size_hint(track.size())
        ),
        url: track.url().as_str().to_owned(),
        filename: track.filename().map(str::to_owned),
        selection: track
            .selection()
            .map(ExecutionSelection::as_str)
            .map(str::to_owned),
    }
}

fn capture_kind_name(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Hls => "HLS",
        ResourceKind::Dash => "DASH",
        ResourceKind::File => "File",
        ResourceKind::Unknown => "Unknown",
    }
}

fn track_kind_name(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "video",
        TrackKind::Audio => "audio",
        TrackKind::Text => "text",
        TrackKind::Unknown => "media",
    }
}

fn size_hint(size: TrackSize) -> String {
    match size {
        TrackSize::Exact(total) => format_size(total),
        TrackSize::Estimated(total) => format!("~{}", format_size(total)),
        TrackSize::Unknown => "size unknown".to_owned(),
    }
}

fn format_size(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit < UNITS.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{amount:.1} {}", UNITS[unit])
    }
}

fn download_id(id: i64) -> Result<DownloadId, String> {
    DownloadId::new(id).map_err(|_| "Invalid download identifier.".into())
}

fn validate_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value.trim()).map_err(|_| "Enter a valid URL.".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("Use an HTTP or HTTPS URL.".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs containing a username or password are not supported.".into());
    }
    Ok(url)
}

fn engine_for(url: &Url, resource: Option<ResourceKind>) -> JobEngine {
    match resource {
        Some(ResourceKind::Hls | ResourceKind::Dash) => JobEngine::Stream,
        Some(_) => JobEngine::Http,
        None if path_names_stream(url) => JobEngine::Stream,
        None => JobEngine::Http,
    }
}

fn path_names_stream(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".mpd")
}

fn job_name(job: &DownloadJob) -> String {
    job.resolved_destination()
        .and_then(|destination| destination.final_path().file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .or_else(|| job.destination().preferred_filename().map(str::to_owned))
        .unwrap_or_else(|| {
            if matches!(
                job.state(),
                DownloadState::Queued | DownloadState::Inspecting
            ) {
                "Preparing download…".into()
            } else {
                "Unnamed file".into()
            }
        })
}

fn state_name(state: DownloadState) -> &'static str {
    match state {
        DownloadState::Queued => "queued",
        DownloadState::Inspecting => "inspecting",
        DownloadState::Downloading => "downloading",
        DownloadState::Paused => "paused",
        DownloadState::Finalizing => "finalizing",
        DownloadState::Completed => "completed",
        DownloadState::Failed => "failed",
        DownloadState::Cancelled => "cancelled",
        DownloadState::Interrupted => "interrupted",
    }
}

fn failure_kind_name(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Network => "network",
        FailureKind::Authentication => "authentication",
        FailureKind::RemoteRejected => "remote_rejected",
        FailureKind::InvalidResponse => "invalid_response",
        FailureKind::UnsupportedResource => "unsupported_resource",
        FailureKind::PermissionDenied => "permission_denied",
        FailureKind::DiskFull => "disk_full",
        FailureKind::Integrity => "integrity",
        FailureKind::Storage => "storage",
        FailureKind::Internal => "internal",
    }
}
