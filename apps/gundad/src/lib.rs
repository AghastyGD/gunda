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
    Candidate, CandidateId, CaptureDownloadStatus, CaptureEvent, CaptureHandle, CaptureServer,
    FormatsResponse, MediaFormat, MediaVariant, PairingToken, Refusal, endpoint_path,
    load_or_create_token, save_endpoint, token_path,
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
use gunda_http::{HttpClient, HttpExecutor, title_filename, url_filename};
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
    last_download_directory: Option<PathBuf>,
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
            } => self
                .download_capture(id, directory, track)
                .await
                .map(|_| ()),
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

        if !captured.variants.is_empty() {
            self.capture_tracks = Some(CaptureTracks {
                id,
                tracks: captured.variants.iter().map(capture_variant_view).collect(),
                problem: None,
            });
            return Ok(());
        }

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
    ) -> Result<DownloadId, String> {
        let captured = self.captured(id)?;
        let directory = directory
            .to_path()
            .map_err(|_| "Invalid destination folder.")?;
        let remembered_directory = directory.clone();
        let (url, track_filename, selection) = match track {
            Some(track) => {
                let url = validate_url(&track.url)?;
                let selection = track
                    .selection
                    .map(ExecutionSelection::new)
                    .transpose()
                    .map_err(|_| "Invalid stream selection.")?;
                (url, track.filename, selection)
            }
            None => (captured.url.clone(), None, None),
        };
        let extension = capture_extension(&captured, track_filename.as_deref());
        let filename = track_filename.clone().or_else(|| {
            preferred_capture_filename(
                captured.filename.as_deref(),
                captured.page_title.as_deref(),
                extension.as_deref(),
            )
        });
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
        self.last_download_directory = Some(remembered_directory);
        self.capture_tracks = None;
        self.inspection_sequence = self.inspection_sequence.saturating_add(1);
        Ok(id)
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
            kind: candidate.kind(),
            content_type: candidate.content_type().map(str::to_owned),
            page_title: candidate.page_title().map(str::to_owned),
            filename: candidate.filename().map(str::to_owned),
            variants: candidate.variants().to_vec(),
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
        last_download_directory: None,
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
            event = capture_event(&mut daemon.capture) => {
                daemon.handle_capture_event(event).await;
                daemon.schedule_jobs();
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
            if save_endpoint(&endpoint_path(directory), &handle.endpoint()).is_err() {
                return (
                    None,
                    Some("The browser capture endpoint could not be published.".into()),
                );
            }
            tokio::spawn(server.run());
            (Some(handle), None)
        }
        Err(_) => (
            None,
            Some("The browser capture channel could not be started.".into()),
        ),
    }
}

async fn capture_event(
    capture: &mut Option<CaptureHandle>,
) -> Result<CaptureEvent, gunda_capture::CaptureError> {
    match capture {
        Some(handle) => handle.next_event().await,
        None => std::future::pending().await,
    }
}

impl Daemon {
    async fn handle_capture_event(
        &mut self,
        event: Result<CaptureEvent, gunda_capture::CaptureError>,
    ) {
        match event {
            Ok(CaptureEvent::Changed) => {}
            Ok(CaptureEvent::FormatsRequested { candidate, reply }) => {
                let formats = self.formats_for_candidate(&candidate).await;
                let _ = reply.send(formats);
            }
            Ok(CaptureEvent::DownloadRequested {
                candidate,
                format,
                directory,
                filename,
                reply,
            }) => {
                let Some(directory) = directory
                    .filter(|directory| !directory.trim().is_empty())
                    .map(PathBuf::from)
                    .or_else(|| self.last_download_directory.clone())
                    .or_else(default_download_directory)
                else {
                    self.notice =
                        Some("Choose a download folder before downloading a captured file.".into());
                    let _ = reply.send(Err(Refusal::CaptureUnavailable));
                    return;
                };
                let Ok(directory) = NativePath::from_path(&directory) else {
                    self.notice = Some("The download folder is invalid.".into());
                    let _ = reply.send(Err(Refusal::CaptureUnavailable));
                    return;
                };
                let choice = capture_choice(&candidate, format.as_deref(), filename);
                let result = match choice {
                    Ok(choice) => {
                        let selected_url = choice
                            .as_ref()
                            .and_then(|choice| Url::parse(&choice.url).ok())
                            .unwrap_or_else(|| candidate.url().clone());
                        let already_active = self.manager.jobs().into_iter().find_map(|job| {
                            (job.request().url() == &selected_url
                                && matches!(
                                    job.state(),
                                    DownloadState::Queued | DownloadState::Downloading
                                ))
                            .then(|| download_id_value(job.id()))
                            .flatten()
                        });
                        if let Some(id) = already_active {
                            Ok(id)
                        } else {
                            match self
                                .download_capture(candidate.id(), directory, choice)
                                .await
                            {
                                Ok(id) => download_id_value(id).ok_or(Refusal::CaptureUnavailable),
                                Err(message) => {
                                    self.notice = Some(message);
                                    Err(Refusal::CaptureUnavailable)
                                }
                            }
                        }
                    }
                    Err(message) => {
                        self.notice = Some(message);
                        Err(Refusal::CaptureUnavailable)
                    }
                };
                let _ = reply.send(result);
            }
            Ok(CaptureEvent::StatusRequested { id, reply }) => {
                let status = i64::try_from(id)
                    .ok()
                    .and_then(|id| DownloadId::new(id).ok())
                    .and_then(|id| self.manager.job(id))
                    .and_then(|job| {
                        Some(CaptureDownloadStatus {
                            id: download_id_value(job.id())?,
                            name: job_name(&job),
                            state: state_name(job.state()).to_owned(),
                            written: job.progress().downloaded_bytes(),
                            total: job.progress().total_bytes(),
                            failure: job
                                .last_failure()
                                .map(|failure| failure_kind_name(failure.kind()).to_owned()),
                        })
                    })
                    .ok_or(Refusal::NoDownload);
                let _ = reply.send(status);
            }
            Err(_) => self.handle_capture_change(false),
        }
    }

    async fn formats_for_candidate(
        &self,
        candidate: &Candidate,
    ) -> Result<FormatsResponse, Refusal> {
        let formats = if candidate.kind() == ResourceKind::File && !candidate.variants().is_empty()
        {
            candidate
                .variants()
                .iter()
                .enumerate()
                .map(|(index, variant)| {
                    let view = capture_variant_view(variant);
                    MediaFormat {
                        label: view.label,
                        group: view.group,
                        details: view.details,
                        format: format!("variant:{index}"),
                    }
                })
                .collect()
        } else if matches!(candidate.kind(), ResourceKind::Hls | ResourceKind::Dash) {
            let request = RequestContext::new(candidate.url().clone(), Vec::new());
            let presentation = self
                .engines
                .stream
                .inspect(&request)
                .await
                .map_err(|_| Refusal::CaptureUnavailable)?;
            presentation
                .tracks()
                .iter()
                .enumerate()
                .map(|(index, track)| {
                    let size = declared_track_size(candidate, track);
                    let mut details = vec![track_kind_name(track.kind()).to_owned()];
                    if let Some(codecs) = track.codecs() {
                        details.push(codecs.to_owned());
                    }
                    if let Some(bytes) = size.or_else(|| track.size().bytes()) {
                        details.push(format_size(bytes));
                    }
                    MediaFormat {
                        label: track.label().to_owned(),
                        group: track.group().map(str::to_owned),
                        details: details.join(" | "),
                        format: track.selection().map_or_else(
                            || format!("track:{index}"),
                            |selection| {
                                format!(
                                    "selection:{}",
                                    selection_with_size(selection.as_str(), size)
                                )
                            },
                        ),
                    }
                })
                .collect()
        } else {
            vec![MediaFormat {
                label: candidate
                    .filename()
                    .or_else(|| candidate.page_title())
                    .unwrap_or("Original file")
                    .to_owned(),
                group: None,
                details: candidate.content_type().unwrap_or("file").to_owned(),
                format: String::from("best"),
            }]
        };

        if formats.is_empty() {
            Err(Refusal::CaptureUnavailable)
        } else {
            let extension = suggested_capture_extension(candidate);

            Ok(FormatsResponse {
                capture_id: candidate.id(),
                formats,
                directory: self
                    .last_download_directory
                    .clone()
                    .or_else(default_download_directory)
                    .and_then(|directory| directory.to_str().map(str::to_owned)),
                filename: preferred_capture_filename(
                    candidate.filename(),
                    candidate.page_title(),
                    Some(&extension),
                ),
            })
        }
    }

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

fn default_download_directory() -> Option<PathBuf> {
    directories::UserDirs::new()
        .and_then(|directories| directories.download_dir().map(Path::to_path_buf))
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
    kind: ResourceKind,
    content_type: Option<String>,
    page_title: Option<String>,
    filename: Option<String>,
    variants: Vec<MediaVariant>,
}

fn filename_extension(filename: &str) -> Option<String> {
    Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_owned)
}

fn preferred_capture_filename(
    filename: Option<&str>,
    page_title: Option<&str>,
    extension: Option<&str>,
) -> Option<String> {
    filename
        .and_then(|filename| title_filename(filename, None))
        .map(|filename| {
            if Path::new(&filename).extension().is_some() {
                filename
            } else if let Some(extension) = extension {
                format!("{filename}.{extension}")
            } else {
                filename
            }
        })
        .or_else(|| page_title.and_then(|title| title_filename(title, extension)))
        .or_else(|| extension.map(|extension| format!("download.{extension}")))
}

fn capture_extension(captured: &Captured, track_filename: Option<&str>) -> Option<String> {
    track_filename
        .and_then(filename_extension)
        .or_else(|| captured.filename.as_deref().and_then(filename_extension))
        .or_else(|| {
            captured
                .content_type
                .as_deref()
                .and_then(content_type_extension)
                .map(str::to_owned)
        })
        .or_else(|| match captured.kind {
            ResourceKind::Hls | ResourceKind::Dash => Some("mp4".into()),
            ResourceKind::File | ResourceKind::Unknown => {
                url_filename(&captured.url).and_then(|filename| filename_extension(&filename))
            }
        })
}

fn content_type_extension(content_type: &str) -> Option<&'static str> {
    match content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"),
        "video/quicktime" => Some("mov"),
        "video/x-flv" | "video/flv" => Some("flv"),
        "video/x-msvideo" | "video/avi" => Some("avi"),
        "video/x-ms-wmv" | "video/wmv" => Some("wmv"),
        "video/mpeg" => Some("mpg"),
        "video/3gpp" => Some("3gp"),
        "video/ogg" => Some("ogv"),
        "audio/mp4" | "audio/x-m4a" => Some("m4a"),
        "audio/mpeg" => Some("mp3"),
        "audio/aac" => Some("aac"),
        "audio/ogg" => Some("ogg"),
        "audio/opus" => Some("opus"),
        "audio/flac" => Some("flac"),
        "audio/wav" | "audio/x-wav" => Some("wav"),
        "application/pdf" => Some("pdf"),
        "application/zip" | "application/x-zip-compressed" => Some("zip"),
        "application/x-7z-compressed" => Some("7z"),
        "application/x-rar-compressed" | "application/vnd.rar" => Some("rar"),
        "application/gzip" | "application/x-gzip" => Some("gz"),
        "application/x-tar" => Some("tar"),
        "application/msword" => Some("doc"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Some("docx"),
        "application/vnd.ms-excel" => Some("xls"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Some("xlsx"),
        "application/vnd.ms-powerpoint" => Some("ppt"),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => Some("pptx"),
        "application/epub+zip" => Some("epub"),
        "application/vnd.android.package-archive" => Some("apk"),
        "application/x-iso9660-image" => Some("iso"),
        "text/plain" => Some("txt"),
        "text/csv" => Some("csv"),
        "application/json" => Some("json"),
        "application/xml" | "text/xml" => Some("xml"),
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        _ => None,
    }
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
        filename: candidate.filename().map(str::to_owned),
        seen: candidate.seen(),
        segmented: matches!(candidate.kind(), ResourceKind::Hls | ResourceKind::Dash),
        selectable: matches!(candidate.kind(), ResourceKind::Hls | ResourceKind::Dash)
            || candidate.variants().len() > 1,
    }
}

fn capture_variant_view(variant: &MediaVariant) -> CaptureTrackView {
    let mut details = vec![String::from("video")];
    if let Some(bitrate) = variant.bitrate() {
        details.push(format!("{} kbps", bitrate / 1000));
    }
    if let Some(codecs) = variant.codecs() {
        details.push(codecs.to_owned());
    }
    if details.len() == 1
        && let Some(content_type) = variant.content_type()
    {
        details.push(content_type.to_owned());
    }

    CaptureTrackView {
        label: variant.label(),
        group: None,
        details: details.join(" | "),
        url: variant.url().as_str().to_owned(),
        filename: None,
        selection: None,
    }
}

fn capture_choice(
    candidate: &Candidate,
    format: Option<&str>,
    filename: Option<String>,
) -> Result<Option<CaptureTrackChoice>, String> {
    let Some(format) = format.filter(|format| !format.is_empty() && *format != "best") else {
        return Ok(filename.map(|filename| CaptureTrackChoice {
            url: candidate.url().as_str().to_owned(),
            filename: Some(filename),
            selection: None,
        }));
    };

    if let Some(index) = format.strip_prefix("variant:") {
        let index = index
            .parse::<usize>()
            .map_err(|_| "That capture format is invalid.".to_string())?;
        let variant = candidate
            .variants()
            .get(index)
            .ok_or_else(|| "That capture format is no longer available.".to_string())?;

        return Ok(Some(CaptureTrackChoice {
            url: variant.url().as_str().to_owned(),
            filename,
            selection: None,
        }));
    }

    if let Some(selection) = format.strip_prefix("selection:") {
        return Ok(Some(CaptureTrackChoice {
            url: candidate.url().as_str().to_owned(),
            filename,
            selection: Some(selection.to_owned()),
        }));
    }

    if format.starts_with("track:") {
        return Ok(filename.map(|filename| CaptureTrackChoice {
            url: candidate.url().as_str().to_owned(),
            filename: Some(filename),
            selection: None,
        }));
    }

    Err("That capture format is invalid.".into())
}

fn suggested_capture_extension(candidate: &Candidate) -> String {
    if matches!(candidate.kind(), ResourceKind::Hls | ResourceKind::Dash) {
        return String::from("mp4");
    }
    candidate
        .filename()
        .and_then(filename_extension)
        .or_else(|| {
            candidate
                .content_type()
                .and_then(content_type_extension)
                .map(str::to_owned)
        })
        .or_else(|| {
            url_filename(candidate.url()).and_then(|filename| filename_extension(&filename))
        })
        .unwrap_or_else(|| String::from("bin"))
}

fn declared_track_size(candidate: &Candidate, track: &PresentationTrack) -> Option<u64> {
    let variants = candidate.variants();
    let audio = variants
        .iter()
        .filter(|variant| variant.resolution().is_none())
        .filter_map(MediaVariant::size)
        .max();

    match track.kind() {
        TrackKind::Video => {
            let video = variants
                .iter()
                .filter(|variant| variant.resolution() == track.resolution())
                .filter_map(MediaVariant::size)
                .max()?;
            Some(video.saturating_add(audio.unwrap_or_default()))
        }
        TrackKind::Audio => audio,
        TrackKind::Text | TrackKind::Unknown => track.size().bytes(),
    }
}

fn selection_with_size(selection: &str, size: Option<u64>) -> String {
    // Only HLS selectors carry an optional declared total; a DASH selector
    // rejects any part it does not define.
    if !selection.starts_with("hls:") {
        return selection.to_owned();
    }

    size.map_or_else(
        || selection.to_owned(),
        |size| format!("{selection}:{size}"),
    )
}

fn capture_track_view(track: &PresentationTrack) -> CaptureTrackView {
    CaptureTrackView {
        label: track.label().to_owned(),
        group: track.group().map(str::to_owned),
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

fn download_id_value(id: DownloadId) -> Option<u64> {
    u64::try_from(id.value()).ok()
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

#[cfg(test)]
mod tests {
    use super::{content_type_extension, filename_extension, preferred_capture_filename};

    #[test]
    fn maps_common_download_types_to_extensions() {
        assert_eq!(
            content_type_extension("application/pdf; charset=binary"),
            Some("pdf")
        );
        assert_eq!(content_type_extension("application/zip"), Some("zip"));
        assert_eq!(
            content_type_extension(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            ),
            Some("docx")
        );
    }

    #[test]
    fn a_suggested_filename_wins_over_the_page_title() {
        assert_eq!(
            preferred_capture_filename(Some("Quarterly report.pdf"), Some("Files"), Some("pdf"))
                .as_deref(),
            Some("Quarterly report.pdf")
        );
        assert_eq!(
            preferred_capture_filename(None, None, Some("pdf")).as_deref(),
            Some("download.pdf")
        );
        assert_eq!(filename_extension("report.pdf").as_deref(), Some("pdf"));
    }
}
