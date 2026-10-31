use std::{
    collections::BTreeSet,
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

#[cfg(test)]
use std::{collections::BTreeMap, sync::Mutex};
#[cfg(test)]
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::Path,
};

use eframe::egui;
use gunda_capture::CandidateId;
#[cfg(test)]
use gunda_capture::{
    Candidate, CaptureError, CaptureHandle, CaptureServer, load_or_create_token, token_path,
};
use gunda_core::download::{DownloadId, DownloadState, ExecutionSelection};
#[cfg(test)]
use gunda_core::{
    application::{
        DownloadCancellation, DownloadExecutor, DownloadManager, DownloadManagerError,
        ExecutionOutput, ExecutionReport,
    },
    download::{
        DownloadDestination, DownloadJob, DownloadOrigin, FileConflictPolicy, NewDownload,
        RequestContext, ResourceDescriptor, ResourceKind,
    },
};
#[cfg(test)]
use gunda_http::{HttpClient, HttpExecutor};
#[cfg(test)]
use gunda_storage::{RuntimeLease, SqliteDownloadRepository};
#[cfg(test)]
use gunda_stream::{PresentationTrack, StreamExecutor, TrackKind, TrackSize};
use tokio::sync::{mpsc, watch};
#[cfg(test)]
use tokio::time::Duration;
use url::Url;

use crate::view::DownloadView;
#[cfg(test)]
use crate::view::bytes;

#[cfg(not(test))]
mod daemon_client;

#[derive(Clone, Default)]
pub struct Snapshot {
    pub ready: bool,
    pub busy: bool,
    pub active: BTreeSet<DownloadId>,
    pub jobs: Vec<DownloadView>,
    pub captures: Vec<CaptureView>,
    pub capture_tracks: Option<CaptureTracks>,
    pub capture: Option<Arc<CaptureStatus>>,
    pub capture_problem: Option<String>,
    pub notice: Option<String>,
}

/// A resource the browser reported.
#[derive(Clone)]
pub struct CaptureView {
    pub id: CandidateId,
    pub url: String,
    pub kind: &'static str,
    pub content_type: Option<String>,
    pub page_title: Option<String>,
    pub filename: Option<String>,
    pub seen: u32,
    /// Whether the resource is a presentation whose tracks can be listed.
    pub segmented: bool,
    /// Whether more than one quality or track can be selected.
    pub selectable: bool,
}

/// The tracks one captured presentation offers, once they have been read.
#[derive(Clone)]
pub struct CaptureTracks {
    pub id: CandidateId,
    pub tracks: Vec<TrackView>,
    pub problem: Option<String>,
}

/// One quality or track the user can choose.
#[derive(Clone)]
pub struct TrackView {
    pub label: String,
    /// Heading that groups qualities sharing the same audio rendition.
    pub group: Option<String>,
    pub details: String,
    pub url: Url,
    pub filename: Option<String>,
    pub selection: Option<ExecutionSelection>,
}

/// A chosen track, carried from the panel to the backend.
#[derive(Clone)]
pub struct TrackChoice {
    pub url: Url,
    pub filename: Option<String>,
    pub selection: Option<ExecutionSelection>,
}

/// What the user needs to pair the browser with this application.
#[derive(Clone)]
pub struct CaptureStatus {
    pub endpoint: String,
    pub token: String,
}

pub enum Command {
    Start {
        url: Url,
        directory: PathBuf,
    },
    Capture {
        id: CandidateId,
        directory: PathBuf,
        track: Option<TrackChoice>,
    },
    Inspect(CandidateId),
    DismissCapture(CandidateId),
    ClearCaptures,
    Resume(DownloadId),
    Retry(DownloadId),
    Restart(DownloadId),
    SaveStatus(DownloadId),
    #[cfg(not(test))]
    Pause(DownloadId),
    #[cfg(not(test))]
    Cancel(DownloadId),
}

#[cfg(test)]
type Active = Arc<Mutex<BTreeMap<DownloadId, DownloadCancellation>>>;

#[cfg(test)]
const MAX_AUTOMATIC_ATTEMPTS: u8 = 3;
#[cfg(test)]
const MAX_CONCURRENT_DOWNLOADS: usize = 3;

#[cfg(test)]
enum WorkerEvent {
    Job {
        job: Box<DownloadJob>,
        pause_supported: bool,
    },
    Notice(String),
    ClearNotice,
    Finished {
        id: DownloadId,
        result: Result<ExecutionReport, DownloadManagerError>,
    },
}

pub struct Backend {
    commands: Option<mpsc::Sender<Command>>,
    pub snapshots: watch::Receiver<Snapshot>,
    #[cfg(test)]
    active: Active,
    closing: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Backend {
    #[cfg(test)]
    pub fn with_snapshot(snapshot: Snapshot) -> Self {
        let (_, snapshots) = watch::channel(snapshot);
        Self {
            commands: None,
            snapshots,
            active: Arc::new(Mutex::new(BTreeMap::new())),
            closing: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    pub fn start(directory: PathBuf, ctx: egui::Context) -> io::Result<Self> {
        #[cfg(test)]
        {
            Self::start_in_process(directory, ctx)
        }

        #[cfg(not(test))]
        {
            daemon_client::start(directory, ctx)
        }
    }

    #[cfg(test)]
    fn start_in_process(directory: PathBuf, ctx: egui::Context) -> io::Result<Self> {
        let (commands, receiver) = mpsc::channel(4);
        let (sender, snapshots) = watch::channel(Snapshot::default());
        let active = Arc::new(Mutex::new(BTreeMap::new()));
        let closing = Arc::new(AtomicBool::new(false));
        let worker_active = active.clone();
        let worker_closing = closing.clone();
        let worker = std::thread::Builder::new()
            .name("gunda-engine".into())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build();
                match result {
                    Ok(runtime) => runtime.block_on(run(
                        directory,
                        receiver,
                        sender,
                        worker_active,
                        worker_closing,
                        ctx,
                    )),
                    Err(_) => publish(
                        &sender,
                        &ctx,
                        Snapshot {
                            notice: Some(
                                "Could not start Gunda. Close the app and try again.".into(),
                            ),
                            ..Default::default()
                        },
                    ),
                }
            })?;
        Ok(Self {
            commands: Some(commands),
            snapshots,
            #[cfg(test)]
            active,
            closing,
            worker: Some(worker),
        })
    }

    pub fn send(&self, command: Command) -> Result<(), &'static str> {
        self.commands
            .as_ref()
            .ok_or("Gunda is closing.")?
            .try_send(command)
            .map_err(|_| "Gunda is busy. Try again in a moment.")
    }

    pub fn cancel(&self, id: DownloadId) -> bool {
        #[cfg(not(test))]
        {
            self.send(Command::Cancel(id)).is_ok()
        }

        #[cfg(test)]
        {
            let Ok(active) = self.active.lock() else {
                return false;
            };
            let Some(cancellation) = active.get(&id).cloned() else {
                return false;
            };

            cancellation.request();
            true
        }
    }

    pub fn pause(&self, id: DownloadId) -> bool {
        #[cfg(not(test))]
        {
            self.send(Command::Pause(id)).is_ok()
        }

        #[cfg(test)]
        {
            let Ok(active) = self.active.lock() else {
                return false;
            };
            let Some(control) = active.get(&id).cloned() else {
                return false;
            };

            control.request_pause()
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        self.commands.take();
        // The process exit aborts active tasks; joining here would block the UI.
        let snapshot = self.snapshots.borrow();
        let idle = !snapshot.busy && snapshot.active.is_empty();
        drop(snapshot);
        if idle && let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn publish(sender: &watch::Sender<Snapshot>, ctx: &egui::Context, snapshot: Snapshot) {
    // Coalesce progress updates so a slow UI cannot accumulate an unbounded queue.
    sender.send_replace(snapshot);
    ctx.request_repaint();
}

/// The port the capture listener prefers, so a pairing survives a restart.
#[cfg(test)]
const CAPTURE_PORT: u16 = 48_123;

/// Everything the command loop needs, once the application can run.
#[cfg(test)]
struct Initialized {
    _lease: RuntimeLease,
    manager: Arc<DownloadManager<SqliteDownloadRepository>>,
    engines: Engines,
    capture: Option<CaptureHandle>,
    capture_problem: Option<String>,
}

/// The engines a download can be executed with.
///
/// A job is executed by exactly one of them: the plain HTTP engine fetches one
/// representation, and the segmented engine reads a manifest and then its
/// segments. Which one applies is a property of the resource, not of the user,
/// so it is decided once, from the URL and from what capture already knew.
#[cfg(test)]
struct Engines {
    http: HttpExecutor,
    stream: StreamExecutor,
}

#[cfg(test)]
async fn initialize(directory: PathBuf) -> Result<Initialized, &'static str> {
    std::fs::create_dir_all(&directory).map_err(|_| "Could not create Gunda's data folder.")?;
    let lease = RuntimeLease::acquire(directory.join("gunda.runtime.lock"))
        .map_err(|_| "Could not open Gunda's data folder. Close any other Gunda window and check folder permissions.")?;
    let repository = SqliteDownloadRepository::open(directory.join("gunda.sqlite3"))
        .await
        .map_err(|_| "Could not open saved downloads. Check the application data folder.")?;
    let store = repository.http_resume_store();
    let stream_store = repository.stream_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .map_err(|_| "Could not load saved downloads.")?;
    let client = HttpClient::new().map_err(|_| "Could not start the download service.")?;
    let stream_client =
        HttpClient::new().map_err(|_| "Could not start the segmented download service.")?;
    let (capture, capture_problem) = start_capture(&directory).await;

    Ok(Initialized {
        _lease: lease,
        manager: Arc::new(manager),
        engines: Engines {
            http: HttpExecutor::with_resume_store(client, store),
            stream: StreamExecutor::with_resume_store(stream_client, stream_store),
        },
        capture,
        capture_problem,
    })
}

/// Starts the capture listener, preferring a port the browser already knows.
///
/// A failure here is not fatal: downloads and the rest of the application work
/// without a browser, and the interface says what is missing.
#[cfg(test)]
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
        // The preferred port is taken. Any free loopback port will do, and the
        // endpoint shown to the user is the one that was actually bound, so the
        // only cost is pairing again.
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

#[cfg(test)]
async fn run(
    directory: PathBuf,
    mut commands: mpsc::Receiver<Command>,
    sender: watch::Sender<Snapshot>,
    active: Active,
    closing: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    let Initialized {
        _lease,
        manager,
        engines,
        capture,
        capture_problem,
    } = match initialize(directory).await {
        Ok(initialized) => initialized,
        Err(message) => {
            publish(
                &sender,
                &ctx,
                Snapshot {
                    notice: Some(message.into()),
                    ..Default::default()
                },
            );
            return;
        }
    };
    let mut pending_outputs = BTreeMap::new();
    let mut active_jobs = BTreeMap::new();
    let mut pending_engines = BTreeMap::new();
    let (worker_tx, mut worker_rx) = mpsc::unbounded_channel();
    let engines = Arc::new(engines);
    let mut snapshot = Snapshot {
        ready: true,
        jobs: idle_views(&manager, &engines, &pending_outputs, &active_jobs).await,
        captures: capture_views(capture.as_ref()),
        capture: capture.as_ref().map(capture_status),
        capture_problem,
        ..Default::default()
    };
    publish(&sender, &ctx, snapshot.clone());

    // Watching the catalog is separate from holding it: `changed` needs the
    // handle mutably, while the loop body reads the candidates as they stand.
    let mut watcher = capture.clone();

    loop {
        enum Wake {
            Command(Option<Box<Command>>),
            Catalog { stopped: bool },
            Worker(Option<Box<WorkerEvent>>),
        }

        let wake = tokio::select! {
            command = commands.recv() => Wake::Command(command.map(Box::new)),
            result = catalog_changed(&mut watcher) => Wake::Catalog { stopped: result.is_err() },
            event = worker_rx.recv() => Wake::Worker(event.map(Box::new)),
        };

        let command = match wake {
            Wake::Catalog { stopped } => {
                if stopped {
                    watcher = None;
                }
                snapshot.captures = capture_views(capture.as_ref());
                publish(&sender, &ctx, snapshot.clone());
                continue;
            }
            Wake::Worker(event) => {
                let mut coordinator = WorkerCoordinator {
                    manager: &manager,
                    engines: &engines,
                    active_jobs: &mut active_jobs,
                    pending_engines: &mut pending_engines,
                    active: &active,
                    pending_outputs: &mut pending_outputs,
                    updates: &worker_tx,
                    closing: &closing,
                    snapshot: &mut snapshot,
                };
                coordinator.handle(event.map(|event| *event)).await;
                publish(&sender, &ctx, snapshot.clone());
                continue;
            }
            Wake::Command(command) => command.map(|command| *command),
        };

        let Some(command) = command else {
            break;
        };
        if closing.load(Ordering::Acquire) {
            break;
        }
        snapshot.busy = true;
        snapshot.notice = None;
        publish(&sender, &ctx, snapshot.clone());
        let id = match command {
            Command::SaveStatus(id) => {
                if let Some(output) = pending_outputs.get(&id) {
                    if manager.confirm_published_output(id, output).await.is_ok() {
                        pending_outputs.remove(&id);
                    } else {
                        snapshot.notice = Some("The file is saved, but its status could not be updated. Try Save status again.".into());
                    }
                } else {
                    snapshot.notice = Some("There is no saved file status to update.".into());
                }
                snapshot.jobs =
                    idle_views(&manager, &engines, &pending_outputs, &active_jobs).await;
                snapshot.busy = false;
                publish(&sender, &ctx, snapshot.clone());
                continue;
            }
            Command::ClearCaptures => {
                if let Some(handle) = &capture
                    && handle.clear().await.is_err()
                {
                    snapshot.notice = Some("The capture list could not be cleared.".into());
                }
                snapshot.busy = false;
                publish(&sender, &ctx, snapshot.clone());
                continue;
            }
            Command::DismissCapture(id) => {
                if let Some(handle) = &capture
                    && handle.remove(id).await.is_err()
                {
                    snapshot.notice = Some("The capture could not be dismissed.".into());
                }
                snapshot.busy = false;
                publish(&sender, &ctx, snapshot.clone());
                continue;
            }
            #[cfg(not(test))]
            Command::Pause(id) => {
                if let Ok(active) = active.lock()
                    && let Some(control) = active.get(&id)
                    && !control.request_pause()
                {
                    snapshot.notice = Some("This download cannot be paused safely.".into());
                }
                snapshot.busy = false;
                publish(&sender, &ctx, snapshot.clone());
                continue;
            }
            #[cfg(not(test))]
            Command::Cancel(id) => {
                if let Ok(active) = active.lock()
                    && let Some(control) = active.get(&id)
                {
                    control.request();
                }
                snapshot.busy = false;
                publish(&sender, &ctx, snapshot.clone());
                continue;
            }
            Command::Start { url, directory } => {
                let engine = engine_for(&url, None);

                manager
                    .create(NewDownload::new(
                        RequestContext::new(url, Vec::new()),
                        DownloadDestination::new(directory, None, FileConflictPolicy::Rename),
                        DownloadOrigin::Desktop,
                    ))
                    .await
                    .map(|event| {
                        let id = event.download_id();
                        pending_engines.insert(id, engine);
                        id
                    })
                    .map_err(|_| "Could not save the new download.")
            }
            Command::Inspect(id) => {
                snapshot.capture_tracks =
                    Some(inspect_capture(&engines.stream, capture.as_ref(), id).await);
                snapshot.busy = false;
                publish(&sender, &ctx, snapshot.clone());
                continue;
            }
            Command::Capture {
                id,
                directory,
                track,
            } => match captured(capture.as_ref(), id) {
                Some(Ok(captured)) => {
                    let engine = captured.engine;

                    // The stream engine re-reads the presentation and resolves
                    // the selector, so the output keeps the presentation's name
                    // rather than a variant playlist's name.
                    let (url, filename, selection) = match track {
                        Some(track) => (track.url, track.filename, track.selection),
                        None => (captured.url, None, None),
                    };

                    snapshot.capture_tracks = None;
                    let mut request = RequestContext::new(url, Vec::new());
                    if let Some(selection) = selection {
                        request = request.with_selection(selection);
                    }

                    manager
                        .create(NewDownload::new(
                            request,
                            DownloadDestination::new(
                                directory,
                                filename,
                                FileConflictPolicy::Rename,
                            ),
                            captured.origin,
                        ))
                        .await
                        .map(|event| {
                            let id = event.download_id();
                            pending_engines.insert(id, engine);
                            id
                        })
                        .map_err(|_| "Could not save the new download.")
                }
                Some(Err(message)) => Err(message),
                None => Err("That capture is no longer in the list."),
            },
            Command::Resume(id) => {
                let engine = engine_of(&manager, id);
                let available = match manager.job(id) {
                    Some(job) => match engine {
                        JobEngine::Http => engines.http.can_resume(&job).await,
                        JobEngine::Stream => engines.stream.can_resume(&job).await,
                    },
                    None => Ok(false),
                };
                match available {
                    Ok(true) => manager
                        .resume(id)
                        .await
                        .map(|_| id)
                        .map_err(|_| "This download cannot be resumed."),
                    Ok(false) => Err(
                        "Saved progress is unavailable. Use Restart to download again from zero.",
                    ),
                    Err(_) => Err(
                        "Could not check saved progress. Check the application data folder and try again.",
                    ),
                }
            }
            Command::Retry(id) => manager
                .retry(id)
                .await
                .map(|_| id)
                .map_err(|_| "This download cannot be retried."),
            Command::Restart(id) => {
                if active_jobs.contains_key(&id) {
                    Err("This download is still running.")
                } else {
                    let engine = engine_of(&manager, id);
                    let directory = manager
                        .job(id)
                        .filter(|job| job.state().can_restart())
                        .map(|job| job.destination().directory().to_path_buf());
                    if let Some(directory) = directory {
                        let reset = match engine {
                            JobEngine::Http => engines
                                .http
                                .reset_for_restart(id, &directory)
                                .await
                                .is_err(),
                            JobEngine::Stream => engines
                                .stream
                                .reset_for_restart(id, &directory)
                                .await
                                .is_err(),
                        };

                        if reset {
                            Err("The previous download data could not be cleared safely.")
                        } else {
                            manager
                                .restart(id)
                                .await
                                .map(|_| id)
                                .map_err(|_| "This download could not be restarted.")
                        }
                    } else {
                        Err("This download cannot be restarted.")
                    }
                }
            }
        };
        match id {
            Ok(id) => {
                if let Some(job) = manager.job(id) {
                    update_job_view(&mut snapshot, &job);
                }
            }
            Err(message) => snapshot.notice = Some(message.into()),
        }
        snapshot.busy = false;
        snapshot.jobs = idle_views(&manager, &engines, &pending_outputs, &active_jobs).await;
        if closing.load(Ordering::Acquire) {
            break;
        }
        schedule_jobs(
            &manager,
            &engines,
            &mut active_jobs,
            &mut pending_engines,
            &active,
            &worker_tx,
            &mut snapshot,
        );
        publish(&sender, &ctx, snapshot.clone());
    }
}

#[cfg(test)]
fn schedule_jobs(
    manager: &Arc<DownloadManager<SqliteDownloadRepository>>,
    engines: &Arc<Engines>,
    active_jobs: &mut BTreeMap<DownloadId, DownloadCancellation>,
    pending_engines: &mut BTreeMap<DownloadId, JobEngine>,
    active: &Active,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
    snapshot: &mut Snapshot,
) {
    while active_jobs.len() < MAX_CONCURRENT_DOWNLOADS {
        let Some(job) = manager.jobs().into_iter().find(|job| {
            job.state() == DownloadState::Queued && !active_jobs.contains_key(&job.id())
        }) else {
            break;
        };
        let id = job.id();
        let engine = pending_engines.remove(&id).unwrap_or_else(|| {
            engine_for(
                job.request().url(),
                job.resource().map(ResourceDescriptor::kind),
            )
        });
        let cancellation = DownloadCancellation::new();

        active_jobs.insert(id, cancellation.clone());
        if let Ok(mut active) = active.lock() {
            active.insert(id, cancellation.clone());
        }
        snapshot.active.insert(id);
        update_job_view(snapshot, &job);

        let manager = Arc::clone(manager);
        let engines = Arc::clone(engines);
        let updates = updates.clone();
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

#[cfg(test)]
struct WorkerCoordinator<'a> {
    manager: &'a Arc<DownloadManager<SqliteDownloadRepository>>,
    engines: &'a Arc<Engines>,
    active_jobs: &'a mut BTreeMap<DownloadId, DownloadCancellation>,
    pending_engines: &'a mut BTreeMap<DownloadId, JobEngine>,
    active: &'a Active,
    pending_outputs: &'a mut BTreeMap<DownloadId, ExecutionOutput>,
    updates: &'a mpsc::UnboundedSender<WorkerEvent>,
    closing: &'a AtomicBool,
    snapshot: &'a mut Snapshot,
}

#[cfg(test)]
impl WorkerCoordinator<'_> {
    async fn handle(&mut self, event: Option<WorkerEvent>) {
        let Some(event) = event else {
            return;
        };

        match event {
            WorkerEvent::Job {
                job,
                pause_supported,
            } => {
                let job = *job;
                update_job_view(self.snapshot, &job);
                if let Some(view) = self
                    .snapshot
                    .jobs
                    .iter_mut()
                    .find(|view| view.id == job.id())
                {
                    view.pause_supported = pause_supported;
                }
            }
            WorkerEvent::Notice(message) => self.snapshot.notice = Some(message),
            WorkerEvent::ClearNotice => self.snapshot.notice = None,
            WorkerEvent::Finished { id, result } => {
                self.active_jobs.remove(&id);
                if let Ok(mut active) = self.active.lock() {
                    active.remove(&id);
                }
                self.snapshot.active.remove(&id);

                if let Some(job) = self.manager.job(id) {
                    update_job_view(self.snapshot, &job);
                }
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
                        self.snapshot.notice = Some("The file was saved. Use Save status to update its download record without downloading again.".into());
                    }
                    Err(_) => {
                        self.snapshot.notice =
                            Some("The download could not be completed. Check its details.".into())
                    }
                    Ok(_) => {}
                }

                self.snapshot.jobs = idle_views(
                    self.manager,
                    self.engines,
                    self.pending_outputs,
                    self.active_jobs,
                )
                .await;
                if !self.closing.load(Ordering::Acquire) {
                    schedule_jobs(
                        self.manager,
                        self.engines,
                        self.active_jobs,
                        self.pending_engines,
                        self.active,
                        self.updates,
                        self.snapshot,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
async fn execute_job<E: DownloadExecutor>(
    manager: &DownloadManager<SqliteDownloadRepository>,
    executor: &E,
    id: DownloadId,
    cancellation: DownloadCancellation,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) -> Result<gunda_core::application::ExecutionReport, DownloadManagerError> {
    let progress_control = cancellation.clone();
    let mut attempts = 0;

    loop {
        attempts += 1;

        let result = manager
            .execute_controlled_with_updates(id, executor, cancellation.clone(), |_, job| {
                let _ = updates.send(WorkerEvent::Job {
                    job: Box::new(job.clone()),
                    pause_supported: progress_control.pause_supported(),
                });
            })
            .await;

        let can_retry = matches!(
            &result,
            Ok(report)
                if matches!(
                    &report.event,
                    gunda_core::application::DownloadEvent::Failed { failure, .. }
                        if failure.is_retryable()
                )
        );
        if !can_retry || attempts >= MAX_AUTOMATIC_ATTEMPTS {
            return result;
        }

        manager.retry(id).await?;

        let delay_seconds = 1_u64 << (attempts - 1);
        let _ = updates.send(WorkerEvent::Notice(format!(
            "Connection problem. Trying again in {delay_seconds} seconds…"
        )));

        tokio::select! {
            biased;
            _ = cancellation.stopped() => {}
            _ = tokio::time::sleep(Duration::from_secs(delay_seconds)) => {}
        }

        let _ = updates.send(WorkerEvent::ClearNotice);
    }
}

/// The engine that can execute a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
enum JobEngine {
    /// One request, one file.
    Http,
    /// A manifest, then its segments.
    Stream,
}

/// What a captured candidate becomes when the user downloads it.
#[cfg(test)]
struct CapturedJob {
    url: Url,
    origin: DownloadOrigin,
    engine: JobEngine,
}

/// Copies everything a job needs out of a captured candidate.
///
/// The copy is taken here because the catalog can evict a candidate while the
/// command waits in the queue.
#[cfg(test)]
fn captured(
    capture: Option<&CaptureHandle>,
    id: CandidateId,
) -> Option<Result<CapturedJob, &'static str>> {
    let handle = capture?;
    let view = handle.view();
    let candidate = view.get(id)?;

    let engine = match capture_engine(candidate) {
        Ok(engine) => engine,
        Err(message) => return Some(Err(message)),
    };

    Some(Ok(CapturedJob {
        url: candidate.url().clone(),
        origin: candidate.origin(),
        engine,
    }))
}

/// Checks that a captured resource can be executed by the engine it needs.
///
/// The segmented engine finds the manifest it needs from the URL. A capture
/// whose URL does not name its format is refused here rather than handed to the
/// plain HTTP engine, which would quietly save a playlist as a file.
#[cfg(test)]
fn capture_engine(candidate: &Candidate) -> Result<JobEngine, &'static str> {
    match candidate.kind() {
        ResourceKind::File => Ok(JobEngine::Http),
        ResourceKind::Hls | ResourceKind::Dash => {
            if looks_segmented(candidate.url()) {
                Ok(JobEngine::Stream)
            } else {
                Err(
                    "This capture is a segmented stream whose URL does not name its format, and the segmented engine cannot be pointed at it yet.",
                )
            }
        }
        ResourceKind::Unknown => Err("This capture is not something Gunda can download."),
    }
}

/// Picks the engine from the recorded resource, or from the URL when unknown.
#[cfg(test)]
fn engine_for(url: &Url, resource: Option<ResourceKind>) -> JobEngine {
    match resource {
        Some(ResourceKind::Hls | ResourceKind::Dash) => JobEngine::Stream,
        Some(_) => JobEngine::Http,
        None if looks_segmented(url) => JobEngine::Stream,
        None => JobEngine::Http,
    }
}

/// The engine a job already in the manager is executed with.
#[cfg(test)]
fn engine_of(manager: &DownloadManager<SqliteDownloadRepository>, id: DownloadId) -> JobEngine {
    manager
        .job(id)
        .map(|job| {
            engine_for(
                job.request().url(),
                job.resource().map(ResourceDescriptor::kind),
            )
        })
        .unwrap_or(JobEngine::Http)
}

/// Whether a URL names a segmented-stream manifest.
#[cfg(test)]
fn looks_segmented(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();

    path.ends_with(".m3u8") || path.ends_with(".mpd")
}

/// Waits until the capture catalog changes.
///
/// With no capture channel there is nothing to wait for, so the future never
/// completes and its branch in the command loop stays quiet.
#[cfg(test)]
async fn catalog_changed(capture: &mut Option<CaptureHandle>) -> Result<(), CaptureError> {
    match capture {
        Some(handle) => handle.changed().await,
        None => std::future::pending().await,
    }
}

/// Describes the tracks a captured presentation offers.
///
/// A capture carries one URL, and a presentation reached that way usually
/// carries several qualities and tracks. Reading them is a deliberate step
/// because it costs requests, so it only happens when the user asks.
#[cfg(test)]
async fn inspect_capture(
    stream: &StreamExecutor,
    capture: Option<&CaptureHandle>,
    id: CandidateId,
) -> CaptureTracks {
    let unavailable = |problem: &str| CaptureTracks {
        id,
        tracks: Vec::new(),
        problem: Some(problem.to_owned()),
    };

    match captured(capture, id) {
        Some(Ok(captured)) => {
            let request = RequestContext::new(captured.url, Vec::new());

            match stream.inspect(&request).await {
                Ok(presentation) => CaptureTracks {
                    id,
                    tracks: presentation
                        .tracks()
                        .iter()
                        .map(TrackView::from_track)
                        .collect(),
                    problem: None,
                },
                Err(_) => unavailable("The qualities of this presentation could not be read."),
            }
        }
        Some(Err(message)) => unavailable(message),
        None => unavailable("That capture is no longer in the list."),
    }
}

impl TrackView {
    #[cfg(test)]
    fn from_track(track: &PresentationTrack) -> Self {
        Self {
            label: track.label().to_owned(),
            group: track.group().map(str::to_owned),
            details: format!("{} | {}", kind_name(track.kind()), size_hint(track.size())),
            url: track.url().clone(),
            filename: track.filename().map(str::to_owned),
            selection: track.selection().cloned(),
        }
    }
}

/// A short name for what a track carries.
#[cfg(test)]
fn kind_name(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "video",
        TrackKind::Audio => "audio",
        TrackKind::Text => "text",
        TrackKind::Unknown => "media",
    }
}

/// What is known about a track's final size.
///
/// An estimate is marked as one: the total a download shows is measured, and
/// the two must not be mistaken for each other.
#[cfg(test)]
fn size_hint(size: TrackSize) -> String {
    match size {
        TrackSize::Exact(total) => bytes(total),
        TrackSize::Estimated(total) => format!("~{}", bytes(total)),
        TrackSize::Unknown => "size unknown".to_owned(),
    }
}

/// Copies the catalog into the shape the interface renders.
#[cfg(test)]
fn capture_views(capture: Option<&CaptureHandle>) -> Vec<CaptureView> {
    let Some(handle) = capture else {
        return Vec::new();
    };

    handle
        .view()
        .candidates()
        .iter()
        .map(CaptureView::from_candidate)
        .collect()
}

impl CaptureView {
    #[cfg(test)]
    fn from_candidate(candidate: &Candidate) -> Self {
        Self {
            id: candidate.id(),
            url: candidate.url().as_str().to_owned(),
            kind: kind_label(candidate.kind()),
            content_type: candidate.content_type().map(str::to_owned),
            page_title: candidate.page_title().map(str::to_owned),
            filename: candidate.filename().map(str::to_owned),
            seen: candidate.seen(),
            segmented: matches!(candidate.kind(), ResourceKind::Hls | ResourceKind::Dash),
            selectable: matches!(candidate.kind(), ResourceKind::Hls | ResourceKind::Dash)
                || candidate.variants().len() > 1,
        }
    }
}

/// How to pair the browser with this application.
#[cfg(test)]
fn capture_status(handle: &CaptureHandle) -> Arc<CaptureStatus> {
    Arc::new(CaptureStatus {
        endpoint: handle.endpoint(),
        token: handle.token().as_str().to_owned(),
    })
}

/// A short label for a captured resource.
#[cfg(test)]
fn kind_label(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Hls => "HLS",
        ResourceKind::Dash => "DASH",
        ResourceKind::File => "File",
        ResourceKind::Unknown => "Unknown",
    }
}

#[cfg(test)]
fn update_job_view(snapshot: &mut Snapshot, job: &DownloadJob) {
    let pause_supported = snapshot
        .jobs
        .iter()
        .find(|view| view.id == job.id())
        .is_some_and(|view| view.pause_supported);
    let mut updated = DownloadView::from_job(job);
    updated.pause_supported = pause_supported;
    if let Some(view) = snapshot.jobs.iter_mut().find(|view| view.id == job.id()) {
        *view = updated;
    } else {
        snapshot.jobs.push(updated);
    }
}

#[cfg(test)]
async fn idle_views(
    manager: &DownloadManager<SqliteDownloadRepository>,
    engines: &Engines,
    pending_outputs: &BTreeMap<DownloadId, ExecutionOutput>,
    active_jobs: &BTreeMap<DownloadId, DownloadCancellation>,
) -> Vec<DownloadView> {
    let mut views = Vec::new();
    for job in manager.jobs() {
        let mut view = DownloadView::from_job(&job);
        let is_active = active_jobs.contains_key(&job.id());
        let availability = if is_active {
            Ok(false)
        } else {
            match engine_of(manager, job.id()) {
                JobEngine::Http => engines.http.can_resume(&job).await,
                JobEngine::Stream => engines.stream.can_resume(&job).await,
            }
        };
        if let Some(cancellation) = active_jobs.get(&job.id()) {
            view.pause_supported = cancellation.pause_supported();
        }
        view.resume_available = matches!(availability, Ok(true));
        if let Some(output) = pending_outputs.get(&job.id()) {
            view.output = Some(output.destination.final_path().to_owned());
            if let Some(name) = output.destination.final_path().file_name() {
                view.name = name.to_string_lossy().into_owned();
            }
            view.completion_pending = true;
        } else if matches!(
            job.state(),
            gunda_core::download::DownloadState::Paused
                | gunda_core::download::DownloadState::Interrupted
        ) && !view.resume_available
        {
            view.error = Some(if availability.is_err() {
                "Could not check saved progress. Check the application data folder and try again."
            } else {
                "Saved progress is unavailable. Use Restart to download again from zero."
            }.into());
        }
        views.push(view);
    }
    views
}

pub fn validate_url(value: &str) -> Result<Url, &'static str> {
    let url = Url::parse(value.trim()).map_err(|_| "Enter a valid URL.")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("Use an HTTP or HTTPS URL.");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs containing a username or password are not supported.");
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gunda_core::{
        application::DownloadRepository,
        download::{
            DownloadJob, DownloadProgress, DownloadState, ResourceDescriptor, ResourceKind,
        },
    };
    use gunda_http::{HttpResumeState, HttpResumeStore, StrongEntityTag, partial_path};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::{Duration, timeout},
    };

    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    mod capture;
    mod recovery;

    async fn wait_for(backend: &mut Backend, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        timeout(TEST_TIMEOUT, async {
            loop {
                let snapshot = backend.snapshots.borrow_and_update().clone();
                if predicate(&snapshot) {
                    return snapshot;
                }
                backend
                    .snapshots
                    .changed()
                    .await
                    .expect("backend must publish an update");
            }
        })
        .await
        .expect("backend must finish within deadline")
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
            assert!(request.len() < 16384);
        }
        request
    }

    async fn serve(response: &'static [u8]) -> (Url, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            timeout(TEST_TIMEOUT, async {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_request(&mut socket).await;
                socket.write_all(response).await.unwrap();
                request
            })
            .await
            .unwrap()
        });
        (
            Url::parse(&format!("http://{address}/file.bin")).unwrap(),
            server,
        )
    }

    async fn stalled_server() -> (
        Url,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, waiting) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let _ = waiting.await;
            socket.write_all(b"hello").await.unwrap();
        });

        (
            Url::parse(&format!("http://{address}/file.bin")).unwrap(),
            release,
            server,
        )
    }

    #[tokio::test]
    async fn two_jobs_transfer_at_the_same_time() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let (first_url, first_release, first_server) = stalled_server().await;
        let (second_url, second_release, second_server) = stalled_server().await;
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();

        wait_for(&mut backend, |snapshot| snapshot.ready).await;
        backend
            .send(Command::Start {
                url: first_url,
                directory: output.path().into(),
            })
            .unwrap();
        backend
            .send(Command::Start {
                url: second_url,
                directory: output.path().into(),
            })
            .unwrap();

        let running = wait_for(&mut backend, |snapshot| {
            snapshot.active.len() == 2
                && snapshot
                    .jobs
                    .iter()
                    .filter(|view| view.state == DownloadState::Downloading)
                    .count()
                    == 2
        })
        .await;
        assert_eq!(running.jobs.len(), 2);

        first_release.send(()).unwrap();
        second_release.send(()).unwrap();
        first_server.await.unwrap();
        second_server.await.unwrap();

        let completed = wait_for(&mut backend, |snapshot| {
            snapshot.active.is_empty()
                && snapshot
                    .jobs
                    .iter()
                    .all(|view| view.state == DownloadState::Completed)
        })
        .await;
        assert_eq!(completed.jobs.len(), 2);
    }

    #[tokio::test]
    async fn concurrency_limit_queues_the_extra_job() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let mut servers = Vec::new();

        for _ in 0..=MAX_CONCURRENT_DOWNLOADS {
            servers.push(stalled_server().await);
        }

        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        wait_for(&mut backend, |snapshot| snapshot.ready).await;

        for (url, _, _) in &servers {
            backend
                .send(Command::Start {
                    url: url.clone(),
                    directory: output.path().into(),
                })
                .unwrap();
        }

        let running = wait_for(&mut backend, |snapshot| {
            snapshot.jobs.len() == MAX_CONCURRENT_DOWNLOADS + 1
                && snapshot.active.len() == MAX_CONCURRENT_DOWNLOADS
                && snapshot
                    .jobs
                    .iter()
                    .filter(|view| view.state == DownloadState::Queued)
                    .count()
                    == 1
        })
        .await;
        assert_eq!(running.jobs.len(), MAX_CONCURRENT_DOWNLOADS + 1);

        let mut handles = Vec::new();
        for (_, release, handle) in servers.drain(..MAX_CONCURRENT_DOWNLOADS) {
            release.send(()).unwrap();
            handles.push(handle);
        }
        for handle in handles {
            handle.await.unwrap();
        }

        let last = wait_for(&mut backend, |snapshot| {
            snapshot.active.len() == 1
                && snapshot
                    .jobs
                    .iter()
                    .filter(|view| view.state == DownloadState::Downloading)
                    .count()
                    == 1
        })
        .await;
        assert_eq!(last.jobs.len(), MAX_CONCURRENT_DOWNLOADS + 1);

        let (_, release, handle) = servers.pop().unwrap();
        release.send(()).unwrap();
        handle.await.unwrap();

        let completed = wait_for(&mut backend, |snapshot| {
            snapshot.active.is_empty()
                && snapshot
                    .jobs
                    .iter()
                    .all(|view| view.state == DownloadState::Completed)
        })
        .await;
        assert_eq!(completed.jobs.len(), MAX_CONCURRENT_DOWNLOADS + 1);
    }

    #[test]
    fn validation_rejects_credentials_and_non_http_input() {
        for value in [
            "",
            "file:///tmp/file.bin",
            "https://user:secret@example.com/file.bin",
        ] {
            assert!(validate_url(value).is_err());
        }
        assert!(validate_url(" https://example.com/file.bin?token=private ").is_ok());
    }

    #[test]
    fn closing_a_busy_backend_is_not_a_user_cancellation() {
        let id = DownloadId::new(1).unwrap();
        let cancellation = DownloadCancellation::new();
        let backend = Backend::with_snapshot(Snapshot {
            busy: true,
            active: BTreeSet::from([id]),
            ..Default::default()
        });
        *backend.active.lock().unwrap() = BTreeMap::from([(id, cancellation.clone())]);
        drop(backend);
        assert!(!cancellation.is_requested());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_backend_during_an_active_transfer_does_not_wait_for_the_engine() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let (url, release, _server) = stalled_server().await;
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();

        wait_for(&mut backend, |snapshot| snapshot.ready).await;
        backend
            .send(Command::Start {
                url,
                directory: output.path().into(),
            })
            .unwrap();
        wait_for(&mut backend, |snapshot| !snapshot.active.is_empty()).await;

        drop(release);
        let dropped = std::thread::spawn(move || drop(backend));
        timeout(Duration::from_secs(2), async { dropped.join().unwrap() })
            .await
            .expect("dropping the backend must not block the UI");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn filename_is_shown_before_body_bytes_arrive_and_updates_after_rename() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let original = output.path().join("manual report.pdf");
        std::fs::write(&original, b"existing file").unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, waiting) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            waiting.await.unwrap();
            socket.write_all(b"hello").await.unwrap();
        });
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        wait_for(&mut backend, |snapshot| snapshot.ready).await;
        backend
            .send(Command::Start {
                url: Url::parse(&format!(
                    "http://{address}/manual%20report.pdf?token=private"
                ))
                .unwrap(),
                directory: output.path().into(),
            })
            .unwrap();
        let running = wait_for(&mut backend, |snapshot| {
            snapshot
                .jobs
                .first()
                .is_some_and(|view| view.state == DownloadState::Downloading)
        })
        .await;
        let view = &running.jobs[0];
        assert!(running.active.contains(&view.id));
        assert_eq!(view.name, "manual report.pdf");
        assert_eq!(view.written, 0);
        assert_eq!(view.total, Some(5));
        assert!(!view.source.contains("private"));
        let repository = SqliteDownloadRepository::open(data.path().join("gunda.sqlite3"))
            .await
            .unwrap();
        let saved = repository.find_by_id(view.id).await.unwrap().unwrap();
        assert_eq!(saved.resolved_destination().unwrap().final_path(), original);
        release.send(()).unwrap();
        let completed = wait_for(&mut backend, |snapshot| {
            !snapshot.busy
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Completed)
        })
        .await;
        let published = completed.jobs[0].output.as_ref().unwrap();
        assert_ne!(published, &original);
        assert_eq!(
            completed.jobs[0].name,
            published.file_name().unwrap().to_string_lossy()
        );
        assert_eq!(std::fs::read(&original).unwrap(), b"existing file");
        assert_eq!(std::fs::read(published).unwrap(), b"hello");
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_download_is_published_and_history_survives_reopen() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let (url, server) =
            serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello").await;
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        wait_for(&mut backend, |snapshot| snapshot.ready).await;
        backend
            .send(Command::Start {
                url,
                directory: output.path().into(),
            })
            .unwrap();
        let snapshot = wait_for(&mut backend, |snapshot| {
            !snapshot.busy
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Completed)
        })
        .await;
        let view = &snapshot.jobs[0];
        assert_eq!(
            std::fs::read(view.output.as_ref().unwrap()).unwrap(),
            b"hello"
        );
        assert_eq!(view.written, 5);
        assert_eq!(view.total, Some(5));
        server.await.unwrap();
        drop(backend);
        let mut reopened = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        let history = wait_for(&mut reopened, |snapshot| snapshot.ready).await;
        assert_eq!(history.jobs[0].state, DownloadState::Completed);
        assert_eq!(history.jobs[0].output, view.output);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restart_downloads_idle_jobs_from_zero_and_preserves_published_files() {
        for (state, keep_output) in [
            (DownloadState::Queued, false),
            (DownloadState::Paused, false),
            (DownloadState::Interrupted, false),
            (DownloadState::Failed, false),
            (DownloadState::Cancelled, false),
            (DownloadState::Completed, false),
            (DownloadState::Completed, true),
        ] {
            let data = tempfile::tempdir().unwrap();
            let output = tempfile::tempdir().unwrap();
            let (url, server) =
                serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
                    .await;
            let repository = SqliteDownloadRepository::open(data.path().join("gunda.sqlite3"))
                .await
                .unwrap();
            let manager = DownloadManager::start(repository).await.unwrap();
            let id = manager
                .create(NewDownload::new(
                    RequestContext::new(url, Vec::new()),
                    DownloadDestination::new(
                        output.path().into(),
                        Some("file.bin".into()),
                        FileConflictPolicy::Rename,
                    ),
                    DownloadOrigin::Desktop,
                ))
                .await
                .unwrap()
                .download_id();
            let mut job = manager.job(id).unwrap();
            let repository = manager.into_repository();
            let published = output.path().join("file.bin");
            std::fs::write(&published, b"previous output").unwrap();
            if !keep_output {
                std::fs::remove_file(&published).unwrap();
            }
            let mut snapshot = job.snapshot();
            snapshot.state = state;
            snapshot.progress = DownloadProgress::new(5, Some(10)).unwrap();
            snapshot.resolved_destination = Some(gunda_core::download::ResolvedDestination::new(
                published.clone(),
            ));
            snapshot.resource = Some(ResourceDescriptor::new(
                ResourceKind::File,
                Some("old file".into()),
                None,
            ));
            if state == DownloadState::Failed {
                snapshot.last_failure = Some(gunda_core::download::DownloadFailure::new(
                    gunda_core::download::FailureKind::Network,
                    "old failure",
                    true,
                ));
            }
            job = DownloadJob::restore(snapshot);
            repository.save(&job).await.unwrap();
            let store = repository.http_resume_store();
            let tag = StrongEntityTag::parse(b"\"old-representation\"").unwrap();
            let initial = store.initialize(id, &tag, 10).await.unwrap();
            let checkpoint = HttpResumeState::new(initial.strong_etag().clone(), 10, 5).unwrap();
            store.save_checkpoint(id, &checkpoint).await.unwrap();
            let partial = partial_path(output.path(), id);
            std::fs::write(&partial, b"stale").unwrap();

            let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
            wait_for(&mut backend, |snapshot| snapshot.ready).await;
            backend.send(Command::Restart(id)).unwrap();
            let completed = wait_for(&mut backend, |snapshot| {
                !snapshot.busy
                    && snapshot.jobs.first().is_some_and(|view| {
                        view.state == DownloadState::Completed && view.total == Some(5)
                    })
            })
            .await;
            let view = &completed.jobs[0];
            assert_eq!(view.id, id);
            assert_eq!(view.written, 5);
            assert_eq!(
                std::fs::read(view.output.as_ref().unwrap()).unwrap(),
                b"hello"
            );
            assert!(view.error.is_none());
            assert!(!partial.exists());
            assert!(store.find(id).await.unwrap().is_none());
            if keep_output {
                assert_eq!(std::fs::read(&published).unwrap(), b"previous output");
                assert_ne!(view.output.as_ref().unwrap(), &published);
            } else {
                assert_eq!(view.output.as_ref().unwrap(), &published);
            }
            let request = String::from_utf8(server.await.unwrap())
                .unwrap()
                .to_ascii_lowercase();
            assert!(!request.contains("\r\nrange:"));
            assert!(!request.contains("\r\nif-range:"));
            let persisted = repository.find_by_id(id).await.unwrap().unwrap();
            assert_eq!(persisted.state(), DownloadState::Completed);
            assert_eq!(persisted.progress().downloaded_bytes(), 5);
            assert!(persisted.last_failure().is_none());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejected_download_has_a_safe_failure_message() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for response in [
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .as_slice(),
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
                    .as_slice(),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                socket.write_all(response).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        let mut url = Url::parse(&format!("http://{address}/file.bin")).unwrap();
        url.set_query(Some("token=private"));
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        wait_for(&mut backend, |snapshot| snapshot.ready).await;
        backend
            .send(Command::Start {
                url,
                directory: output.path().into(),
            })
            .unwrap();
        let snapshot = wait_for(&mut backend, |snapshot| {
            !snapshot.busy
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Failed)
        })
        .await;
        let view = &snapshot.jobs[0];
        let error = view.error.as_deref().unwrap();
        assert!(error.contains("HTTP 403 Forbidden"));
        assert!(!error.contains("private"));
        assert!(!view.source.contains("private"));
        assert!(view.output.is_none());

        let id = view.id;
        let partial = gunda_http::partial_path(output.path(), id);
        std::fs::write(&partial, b"old partial").unwrap();
        backend.send(Command::Restart(id)).unwrap();
        let restarted = wait_for(&mut backend, |snapshot| {
            !snapshot.busy
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Completed)
        })
        .await;
        let restarted_view = &restarted.jobs[0];
        assert_eq!(restarted_view.written, 5);
        assert_eq!(
            std::fs::read(restarted_view.output.as_ref().unwrap()).unwrap(),
            b"hello"
        );
        assert!(!partial.exists());
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retryable_failure_is_retried_automatically() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for response in [
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .as_slice(),
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
                    .as_slice(),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                socket.write_all(response).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        let url = Url::parse(&format!("http://{address}/file.bin")).unwrap();
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        wait_for(&mut backend, |snapshot| snapshot.ready).await;
        backend
            .send(Command::Start {
                url,
                directory: output.path().into(),
            })
            .unwrap();

        let completed = wait_for(&mut backend, |snapshot| {
            !snapshot.busy
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Completed)
        })
        .await;
        let view = &completed.jobs[0];
        assert_eq!(view.written, 5);
        assert_eq!(
            std::fs::read(view.output.as_ref().unwrap()).unwrap(),
            b"hello"
        );
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn automatic_retries_stop_after_the_configured_attempt_limit() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..MAX_AUTOMATIC_ATTEMPTS {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                socket
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        wait_for(&mut backend, |snapshot| snapshot.ready).await;
        backend
            .send(Command::Start {
                url: Url::parse(&format!("http://{address}/file.bin")).unwrap(),
                directory: output.path().into(),
            })
            .unwrap();

        let failed = wait_for(&mut backend, |snapshot| {
            !snapshot.busy && snapshot.jobs.first().is_some_and(DownloadView::can_retry)
        })
        .await;

        assert_eq!(failed.jobs[0].written, 0);
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_during_automatic_retry_backoff_stops_another_request() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
            timeout(Duration::from_millis(1500), listener.accept())
                .await
                .is_err()
        });
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        wait_for(&mut backend, |snapshot| snapshot.ready).await;
        backend
            .send(Command::Start {
                url: Url::parse(&format!("http://{address}/file.bin")).unwrap(),
                directory: output.path().into(),
            })
            .unwrap();
        let retrying = wait_for(&mut backend, |snapshot| {
            snapshot
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Trying again"))
        })
        .await;
        let id = retrying.jobs[0].id;
        assert!(backend.cancel(id));

        let cancelled = wait_for(&mut backend, |snapshot| {
            !snapshot.busy
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Cancelled)
        })
        .await;

        assert_eq!(cancelled.jobs[0].written, 0);
        assert!(
            server.await.unwrap(),
            "backoff must not issue another request"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_reaches_the_transfer_without_waiting_for_the_command_loop() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = Url::parse(&format!(
            "http://{}/file.bin",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let (received, request_received) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            timeout(TEST_TIMEOUT, async {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n")
                    .await
                    .unwrap();
                received.send(()).unwrap();
                released.await.unwrap();
                let _ = socket.write_all(b"hello").await;
            })
            .await
            .unwrap();
        });
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        wait_for(&mut backend, |snapshot| snapshot.ready).await;
        backend
            .send(Command::Start {
                url,
                directory: output.path().into(),
            })
            .unwrap();
        timeout(TEST_TIMEOUT, request_received)
            .await
            .unwrap()
            .unwrap();
        let snapshot = wait_for(&mut backend, |snapshot| {
            !snapshot.active.is_empty()
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Downloading)
        })
        .await;
        let id = *snapshot.active.iter().next().unwrap();
        assert!(!snapshot.jobs[0].pause_supported);
        assert!(!backend.pause(id));
        assert!(backend.cancel(id));
        let snapshot = wait_for(&mut backend, |snapshot| {
            !snapshot.busy
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Cancelled)
        })
        .await;
        assert!(snapshot.jobs[0].output.is_none());
        release.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn active_download_can_pause_and_resume_from_its_checkpoint() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut initial, _) = listener.accept().await.unwrap();
            read_request(&mut initial).await;
            initial
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nETag: \"version-63\"\r\nConnection: close\r\n\r\nhello",
                )
                .await
                .unwrap();
            released.await.unwrap();
            drop(initial);

            let (mut resumed, _) = listener.accept().await.unwrap();
            let request = read_request(&mut resumed).await;
            resumed
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 5-9/10\r\nContent-Length: 5\r\nETag: \"version-63\"\r\nConnection: close\r\n\r\nworld",
                )
                .await
                .unwrap();
            resumed.shutdown().await.unwrap();
            request
        });
        let url = Url::parse(&format!("http://{address}/file.bin")).unwrap();
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        wait_for(&mut backend, |snapshot| snapshot.ready).await;
        backend
            .send(Command::Start {
                url,
                directory: output.path().into(),
            })
            .unwrap();

        let downloading = wait_for(&mut backend, |snapshot| {
            snapshot
                .jobs
                .first()
                .is_some_and(|view| view.state == DownloadState::Downloading && view.written == 5)
        })
        .await;
        let id = downloading.jobs[0].id;
        assert!(downloading.jobs[0].pause_supported);
        assert!(backend.pause(id));

        let paused = wait_for(&mut backend, |snapshot| {
            !snapshot.busy
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Paused)
        })
        .await;
        assert_eq!(paused.jobs[0].written, 5);
        assert!(paused.jobs[0].can_resume());
        assert!(paused.jobs[0].output.is_none());

        release.send(()).unwrap();
        backend.send(Command::Resume(id)).unwrap();

        let completed = wait_for(&mut backend, |snapshot| {
            !snapshot.busy
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Completed)
        })
        .await;
        assert_eq!(completed.jobs[0].written, 10);
        assert_eq!(
            std::fs::read(completed.jobs[0].output.as_ref().unwrap()).unwrap(),
            b"helloworld"
        );
        let request = String::from_utf8(server.await.unwrap())
            .unwrap()
            .to_ascii_lowercase();
        assert!(request.contains("range: bytes=5-\r\n"));
        assert!(request.contains("if-range: \"version-63\"\r\n"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn startup_recovers_and_resumes_from_the_durable_offset() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let (url, server) = serve(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-4/5\r\nContent-Length: 3\r\nETag: \"version-1\"\r\nConnection: close\r\n\r\nllo").await;
        let repository = SqliteDownloadRepository::open(data.path().join("gunda.sqlite3"))
            .await
            .unwrap();
        let manager = DownloadManager::start(repository).await.unwrap();
        let id = manager
            .create(NewDownload::new(
                RequestContext::new(url, Vec::new()),
                DownloadDestination::new(
                    output.path().into(),
                    Some("file.bin".into()),
                    FileConflictPolicy::Rename,
                ),
                DownloadOrigin::Desktop,
            ))
            .await
            .unwrap()
            .download_id();
        let mut snapshot = manager.job(id).unwrap().snapshot();
        snapshot.state = DownloadState::Downloading;
        snapshot.progress = DownloadProgress::new(4, Some(5)).unwrap();
        snapshot.resource = Some(ResourceDescriptor::new(ResourceKind::File, None, None));
        snapshot.resolved_destination = Some(gunda_core::download::ResolvedDestination::new(
            output.path().join("file.bin"),
        ));
        let repository = manager.into_repository();
        repository
            .save(&DownloadJob::restore(snapshot))
            .await
            .unwrap();
        let store = repository.http_resume_store();
        let tag = StrongEntityTag::parse(b"\"version-1\"").unwrap();
        store.initialize(id, &tag, 5).await.unwrap();
        store
            .save_checkpoint(id, &HttpResumeState::new(tag, 5, 2).unwrap())
            .await
            .unwrap();
        let partial = std::fs::File::create(partial_path(output.path(), id)).unwrap();
        use std::io::Write;
        (&partial).write_all(b"heXX").unwrap();
        partial.sync_all().unwrap();
        drop(partial);
        drop(store);
        repository.close().await;
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        let recovered = wait_for(&mut backend, |snapshot| snapshot.ready).await;
        assert_eq!(recovered.jobs[0].state, DownloadState::Interrupted);
        backend.send(Command::Resume(id)).unwrap();
        let completed = wait_for(&mut backend, |snapshot| {
            !snapshot.busy && snapshot.jobs[0].state == DownloadState::Completed
        })
        .await;
        assert_eq!(
            std::fs::read(completed.jobs[0].output.as_ref().unwrap()).unwrap(),
            b"hello"
        );
        let request = String::from_utf8(server.await.unwrap())
            .unwrap()
            .to_ascii_lowercase();
        assert!(request.contains("range: bytes=2-"));
        assert!(request.contains("if-range: \"version-1\""));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_backend_cannot_share_the_runtime_directory() {
        let data = tempfile::tempdir().unwrap();
        let mut first = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        wait_for(&mut first, |snapshot| snapshot.ready).await;
        let mut second = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        let failure = wait_for(&mut second, |snapshot| snapshot.notice.is_some()).await;
        assert!(!failure.ready);
    }
}
