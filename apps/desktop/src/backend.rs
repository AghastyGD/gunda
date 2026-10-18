use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use eframe::egui;
use gunda_capture::{
    Candidate, CandidateId, CaptureError, CaptureHandle, CaptureServer, load_or_create_token,
    token_path,
};
use gunda_core::{
    application::{
        DownloadCancellation, DownloadExecutor, DownloadManager, DownloadManagerError,
        ExecutionOutput,
    },
    download::{
        DownloadDestination, DownloadId, DownloadJob, DownloadOrigin, FileConflictPolicy,
        NewDownload, RequestContext, ResourceDescriptor, ResourceKind,
    },
};
use gunda_http::{HttpClient, HttpExecutor};
use gunda_storage::{RuntimeLease, SqliteDownloadRepository};
use gunda_stream::{PresentationTrack, StreamExecutor, TrackKind, TrackSize};
use tokio::sync::{mpsc, watch};
use tokio::time::Duration;
use url::Url;

use crate::view::{DownloadView, bytes};

#[derive(Clone, Default)]
pub struct Snapshot {
    pub ready: bool,
    pub busy: bool,
    pub active: Option<DownloadId>,
    pub pause_supported: bool,
    pub jobs: Vec<DownloadView>,
    pub captures: Vec<CaptureView>,
    pub capture_tracks: Option<CaptureTracks>,
    pub capture: Option<Arc<CaptureStatus>>,
    pub capture_problem: Option<&'static str>,
    pub notice: Option<String>,
}

/// A media resource the browser reported.
#[derive(Clone)]
pub struct CaptureView {
    pub id: CandidateId,
    pub url: String,
    pub kind: &'static str,
    pub content_type: Option<String>,
    pub page_title: Option<String>,
    pub seen: u32,
    /// Whether the resource is a presentation whose tracks can be listed.
    pub segmented: bool,
}

/// The tracks one captured presentation offers, once they have been read.
#[derive(Clone)]
pub struct CaptureTracks {
    pub id: CandidateId,
    pub tracks: Vec<TrackView>,
    pub problem: Option<&'static str>,
}

/// One quality or track the user can choose.
#[derive(Clone)]
pub struct TrackView {
    pub label: String,
    pub details: String,
    pub url: Url,
    pub filename: Option<String>,
}

/// A chosen track, carried from the panel to the backend.
#[derive(Clone)]
pub struct TrackChoice {
    pub url: Url,
    pub filename: Option<String>,
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
}

type Active = Arc<Mutex<Option<(DownloadId, DownloadCancellation)>>>;

const MAX_AUTOMATIC_ATTEMPTS: u8 = 3;

pub struct Backend {
    commands: Option<mpsc::Sender<Command>>,
    pub snapshots: watch::Receiver<Snapshot>,
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
            active: Arc::new(Mutex::new(None)),
            closing: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    pub fn start(directory: PathBuf, ctx: egui::Context) -> std::io::Result<Self> {
        let (commands, receiver) = mpsc::channel(4);
        let (sender, snapshots) = watch::channel(Snapshot::default());
        let active = Arc::new(Mutex::new(None));
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
        if let Ok(active) = self.active.lock()
            && let Some((active_id, cancellation)) = active.as_ref()
            && *active_id == id
        {
            cancellation.request();
            return true;
        }
        false
    }

    pub fn pause(&self, id: DownloadId) -> bool {
        if let Ok(active) = self.active.lock()
            && let Some((active_id, control)) = active.as_ref()
            && *active_id == id
        {
            return control.request_pause();
        }
        false
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        self.commands.take();
        // Closing the process interrupts a transfer; it must not become a user cancellation.
        let busy = self.snapshots.borrow().busy;
        if !busy && let Some(worker) = self.worker.take() {
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
const CAPTURE_PORT: u16 = 48_123;

/// Everything the command loop needs, once the application can run.
struct Initialized {
    _lease: RuntimeLease,
    manager: DownloadManager<SqliteDownloadRepository>,
    engines: Engines,
    capture: Option<CaptureHandle>,
    capture_problem: Option<&'static str>,
}

/// The engines a download can be executed with.
///
/// A job is executed by exactly one of them: the plain HTTP engine fetches one
/// representation, and the segmented engine reads a manifest and then its
/// segments. Which one applies is a property of the resource, not of the user,
/// so it is decided once, from the URL and from what capture already knew.
struct Engines {
    http: HttpExecutor,
    stream: StreamExecutor,
}

async fn initialize(directory: PathBuf) -> Result<Initialized, &'static str> {
    std::fs::create_dir_all(&directory).map_err(|_| "Could not create Gunda's data folder.")?;
    let lease = RuntimeLease::acquire(directory.join("gunda.runtime.lock"))
        .map_err(|_| "Could not open Gunda's data folder. Close any other Gunda window and check folder permissions.")?;
    let repository = SqliteDownloadRepository::open(directory.join("gunda.sqlite3"))
        .await
        .map_err(|_| "Could not open saved downloads. Check the application data folder.")?;
    let store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .map_err(|_| "Could not load saved downloads.")?;
    let client = HttpClient::new().map_err(|_| "Could not start the download service.")?;
    let stream_client =
        HttpClient::new().map_err(|_| "Could not start the segmented download service.")?;
    let (capture, capture_problem) = start_capture(&directory).await;

    Ok(Initialized {
        _lease: lease,
        manager,
        engines: Engines {
            http: HttpExecutor::with_resume_store(client, store),
            stream: StreamExecutor::new(stream_client),
        },
        capture,
        capture_problem,
    })
}

/// Starts the capture listener, preferring a port the browser already knows.
///
/// A failure here is not fatal: downloads and the rest of the application work
/// without a browser, and the interface says what is missing.
async fn start_capture(directory: &Path) -> (Option<CaptureHandle>, Option<&'static str>) {
    let token = match load_or_create_token(&token_path(directory)) {
        Ok(token) => token,
        Err(_) => {
            return (
                None,
                Some(
                    "The browser capture channel could not be set up. Check the application data folder.",
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
            Some("The browser capture channel could not be started."),
        ),
    }
}

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
        mut manager,
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
    let mut snapshot = Snapshot {
        ready: true,
        jobs: idle_views(&manager, &engines.http, &pending_outputs).await,
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
            Command(Option<Command>),
            Catalog { stopped: bool },
        }

        let wake = tokio::select! {
            command = commands.recv() => Wake::Command(command),
            result = catalog_changed(&mut watcher) => Wake::Catalog { stopped: result.is_err() },
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
            Wake::Command(command) => command,
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
        let (id, engine) = match command {
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
                snapshot.jobs = idle_views(&manager, &engines.http, &pending_outputs).await;
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
            Command::Start { url, directory } => (
                manager
                    .create(NewDownload::new(
                        RequestContext::new(url.clone(), Vec::new()),
                        DownloadDestination::new(directory, None, FileConflictPolicy::Rename),
                        DownloadOrigin::Desktop,
                    ))
                    .await
                    .map(|event| event.download_id())
                    .map_err(|_| "Could not save the new download."),
                engine_for(&url, None),
            ),
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

                    // A chosen track is one playlist of the same presentation,
                    // so the same engine executes it, and it is still named
                    // after the presentation rather than after its playlist.
                    let (url, filename) = match track {
                        Some(track) => (track.url, track.filename),
                        None => (captured.url, None),
                    };

                    snapshot.capture_tracks = None;

                    (
                        manager
                            .create(NewDownload::new(
                                RequestContext::new(url, Vec::new()),
                                DownloadDestination::new(
                                    directory,
                                    filename,
                                    FileConflictPolicy::Rename,
                                ),
                                captured.origin,
                            ))
                            .await
                            .map(|event| event.download_id())
                            .map_err(|_| "Could not save the new download."),
                        engine,
                    )
                }
                Some(Err(message)) => (Err(message), JobEngine::Http),
                None => (
                    Err("That capture is no longer in the list."),
                    JobEngine::Http,
                ),
            },
            Command::Resume(id) => {
                let engine = engine_of(&manager, id);
                let available = match manager.job(id) {
                    Some(job) => engines.http.can_resume(job).await,
                    None => Ok(false),
                };
                (
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
                    },
                    engine,
                )
            }
            Command::Retry(id) => (
                manager
                    .retry(id)
                    .await
                    .map(|_| id)
                    .map_err(|_| "This download cannot be retried."),
                engine_of(&manager, id),
            ),
            Command::Restart(id) => {
                let engine = engine_of(&manager, id);
                let directory = manager
                    .job(id)
                    .filter(|job| job.state().can_restart())
                    .map(|job| job.destination().directory().to_path_buf());
                let restarted = if let Some(directory) = directory {
                    if engines
                        .http
                        .reset_for_restart(id, &directory)
                        .await
                        .is_err()
                    {
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
                };
                (restarted, engine)
            }
        };
        if let Ok(id) = id {
            let cancellation = DownloadCancellation::new();
            if let Ok(mut slot) = active.lock() {
                *slot = Some((id, cancellation.clone()));
            }
            if closing.load(Ordering::Acquire) {
                let _ = manager.pause(id).await;
                break;
            }
            snapshot.active = Some(id);
            snapshot.pause_supported = false;
            if let Some(job) = manager.job(id) {
                update_job_view(&mut snapshot, job);
            }
            publish(&sender, &ctx, snapshot.clone());
            let result = {
                let mut publisher = Publisher {
                    snapshot: &mut snapshot,
                    sender: &sender,
                    ctx: &ctx,
                };

                match engine {
                    JobEngine::Http => {
                        execute_job(
                            &mut manager,
                            &engines.http,
                            id,
                            cancellation.clone(),
                            &mut publisher,
                        )
                        .await
                    }
                    JobEngine::Stream => {
                        execute_job(
                            &mut manager,
                            &engines.stream,
                            id,
                            cancellation.clone(),
                            &mut publisher,
                        )
                        .await
                    }
                }
            };

            if let Ok(mut slot) = active.lock() {
                *slot = None;
            }
            match result {
                Err(DownloadManagerError::CompletionNotPersisted {
                    destination,
                    written_bytes,
                    cleanup_pending,
                    ..
                }) => {
                    pending_outputs.insert(
                        id,
                        ExecutionOutput {
                            destination,
                            written_bytes,
                            cleanup_pending,
                        },
                    );
                    snapshot.notice = Some("The file was saved. Use Save status to update its download record without downloading again.".into());
                }
                Err(_) => {
                    snapshot.notice =
                        Some("The download could not be completed. Check its details.".into())
                }
                Ok(_) => {}
            }
        } else if let Err(message) = id {
            snapshot.notice = Some(message.into());
        }
        snapshot.busy = false;
        snapshot.active = None;
        snapshot.pause_supported = false;
        snapshot.jobs = idle_views(&manager, &engines.http, &pending_outputs).await;
        publish(&sender, &ctx, snapshot.clone());
    }
}

/// The parts of the command loop that publishing a snapshot needs.
struct Publisher<'a> {
    snapshot: &'a mut Snapshot,
    sender: &'a watch::Sender<Snapshot>,
    ctx: &'a egui::Context,
}

impl Publisher<'_> {
    /// Publishes the snapshot and wakes the interface.
    fn publish(&self) {
        publish(self.sender, self.ctx, self.snapshot.clone());
    }
}

/// Runs one job to a conclusion, retrying a failure that is worth retrying.
///
/// This is generic over the engine on purpose: the retry policy, the progress
/// publication, and the pause handling are the same whether a job is one HTTP
/// response or a manifest and a hundred segments.
async fn execute_job<E: DownloadExecutor>(
    manager: &mut DownloadManager<SqliteDownloadRepository>,
    executor: &E,
    id: DownloadId,
    cancellation: DownloadCancellation,
    publisher: &mut Publisher<'_>,
) -> Result<gunda_core::application::ExecutionReport, DownloadManagerError> {
    let progress_control = cancellation.clone();
    let mut attempts = 0;

    loop {
        attempts += 1;

        let result = manager
            .execute_controlled_with_updates(id, executor, cancellation.clone(), |_, job| {
                publisher.snapshot.pause_supported = progress_control.pause_supported();
                update_job_view(publisher.snapshot, job);
                publisher.publish();
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
        if let Some(job) = manager.job(id) {
            update_job_view(publisher.snapshot, job);
        }
        publisher.snapshot.notice = Some(format!(
            "Connection problem. Trying again in {delay_seconds} seconds…"
        ));
        publisher.publish();

        tokio::select! {
            biased;
            _ = cancellation.stopped() => {}
            _ = tokio::time::sleep(Duration::from_secs(delay_seconds)) => {}
        }

        publisher.snapshot.notice = None;
    }
}

/// The engine that can execute a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobEngine {
    /// One request, one file.
    Http,
    /// A manifest, then its segments.
    Stream,
}

/// What a captured candidate becomes when the user downloads it.
struct CapturedJob {
    url: Url,
    origin: DownloadOrigin,
    engine: JobEngine,
}

/// Copies everything a job needs out of a captured candidate.
///
/// The copy is taken here because the catalog can evict a candidate while the
/// command waits in the queue.
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
fn engine_for(url: &Url, resource: Option<ResourceKind>) -> JobEngine {
    match resource {
        Some(ResourceKind::Hls | ResourceKind::Dash) => JobEngine::Stream,
        Some(_) => JobEngine::Http,
        None if looks_segmented(url) => JobEngine::Stream,
        None => JobEngine::Http,
    }
}

/// The engine a job already in the manager is executed with.
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
fn looks_segmented(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();

    path.ends_with(".m3u8") || path.ends_with(".mpd")
}

/// Waits until the capture catalog changes.
///
/// With no capture channel there is nothing to wait for, so the future never
/// completes and its branch in the command loop stays quiet.
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
async fn inspect_capture(
    stream: &StreamExecutor,
    capture: Option<&CaptureHandle>,
    id: CandidateId,
) -> CaptureTracks {
    let unavailable = |problem| CaptureTracks {
        id,
        tracks: Vec::new(),
        problem: Some(problem),
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
    fn from_track(track: &PresentationTrack) -> Self {
        Self {
            label: track.label().to_owned(),
            details: format!("{} · {}", kind_name(track.kind()), size_hint(track.size())),
            url: track.url().clone(),
            filename: track.filename().map(str::to_owned),
        }
    }
}

/// A short name for what a track carries.
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
fn size_hint(size: TrackSize) -> String {
    match size {
        TrackSize::Exact(total) => bytes(total),
        TrackSize::Estimated(total) => format!("~{}", bytes(total)),
        TrackSize::Unknown => "size unknown".to_owned(),
    }
}

/// Copies the catalog into the shape the interface renders.
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
    fn from_candidate(candidate: &Candidate) -> Self {
        Self {
            id: candidate.id(),
            url: candidate.url().as_str().to_owned(),
            kind: kind_label(candidate.kind()),
            content_type: candidate.content_type().map(str::to_owned),
            page_title: candidate.page_title().map(str::to_owned),
            seen: candidate.seen(),
            segmented: matches!(candidate.kind(), ResourceKind::Hls | ResourceKind::Dash),
        }
    }
}

/// How to pair the browser with this application.
fn capture_status(handle: &CaptureHandle) -> Arc<CaptureStatus> {
    Arc::new(CaptureStatus {
        endpoint: handle.endpoint(),
        token: handle.token().as_str().to_owned(),
    })
}

/// A short label for a captured resource.
fn kind_label(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Hls => "HLS",
        ResourceKind::Dash => "DASH",
        ResourceKind::File => "File",
        ResourceKind::Unknown => "Unknown",
    }
}

fn update_job_view(snapshot: &mut Snapshot, job: &DownloadJob) {
    let updated = DownloadView::from_job(job);
    if let Some(view) = snapshot.jobs.iter_mut().find(|view| view.id == job.id()) {
        *view = updated;
    } else {
        snapshot.jobs.push(updated);
    }
}

async fn idle_views(
    manager: &DownloadManager<SqliteDownloadRepository>,
    executor: &HttpExecutor,
    pending_outputs: &BTreeMap<DownloadId, ExecutionOutput>,
) -> Vec<DownloadView> {
    let mut views = Vec::new();
    for job in manager.jobs() {
        let mut view = DownloadView::from_job(job);
        let availability = executor.can_resume(job).await;
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
            active: Some(id),
            ..Default::default()
        });
        *backend.active.lock().unwrap() = Some((id, cancellation.clone()));
        drop(backend);
        assert!(!cancellation.is_requested());
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
        assert!(running.busy);
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
            let mut manager = DownloadManager::start(repository).await.unwrap();
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
            let mut job = manager.job(id).unwrap().clone();
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
            snapshot.active.is_some()
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Downloading)
        })
        .await;
        assert!(!snapshot.pause_supported);
        assert!(!backend.pause(snapshot.active.unwrap()));
        assert!(backend.cancel(snapshot.active.unwrap()));
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
            snapshot.busy
                && snapshot.jobs.first().is_some_and(|view| {
                    view.state == DownloadState::Downloading && view.written == 5
                })
        })
        .await;
        let id = downloading.jobs[0].id;
        assert!(downloading.pause_supported);
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
        let mut manager = DownloadManager::start(repository).await.unwrap();
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
