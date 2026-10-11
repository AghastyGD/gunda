use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use eframe::egui;
use gunda_core::{
    application::{DownloadCancellation, DownloadManager, DownloadManagerError},
    download::{
        DownloadDestination, DownloadId, DownloadOrigin, FileConflictPolicy, NewDownload,
        RequestContext,
    },
};
use gunda_http::{HttpClient, HttpExecutor};
use gunda_storage::{RuntimeLease, SqliteDownloadRepository};
use tokio::sync::{mpsc, watch};
use url::Url;

use crate::view::DownloadView;

#[derive(Clone, Default)]
pub struct Snapshot {
    pub ready: bool,
    pub busy: bool,
    pub active: Option<DownloadId>,
    pub jobs: Vec<DownloadView>,
    pub notice: Option<String>,
}

pub enum Command {
    Start { url: Url, directory: PathBuf },
    Resume(DownloadId),
}

type Active = Arc<Mutex<Option<(DownloadId, DownloadCancellation)>>>;

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

async fn initialize(
    directory: PathBuf,
) -> Result<
    (
        RuntimeLease,
        DownloadManager<SqliteDownloadRepository>,
        HttpExecutor,
    ),
    &'static str,
> {
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
    Ok((
        lease,
        manager,
        HttpExecutor::with_resume_store(client, store),
    ))
}

async fn run(
    directory: PathBuf,
    mut commands: mpsc::Receiver<Command>,
    sender: watch::Sender<Snapshot>,
    active: Active,
    closing: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    let (_lease, mut manager, executor) = match initialize(directory).await {
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
    let mut snapshot = Snapshot {
        ready: true,
        jobs: manager.jobs().map(DownloadView::from_job).collect(),
        ..Default::default()
    };
    publish(&sender, &ctx, snapshot.clone());
    while let Some(command) = commands.recv().await {
        if closing.load(Ordering::Acquire) {
            break;
        }
        snapshot.busy = true;
        snapshot.notice = None;
        publish(&sender, &ctx, snapshot.clone());
        let id = match command {
            Command::Start { url, directory } => manager
                .create(NewDownload::new(
                    RequestContext::new(url, Vec::new()),
                    DownloadDestination::new(directory, None, FileConflictPolicy::Rename),
                    DownloadOrigin::Desktop,
                ))
                .await
                .map(|event| event.download_id())
                .map_err(|_| "Could not save the new download."),
            Command::Resume(id) => manager
                .resume(id)
                .await
                .map(|_| id)
                .map_err(|_| "This download cannot be resumed."),
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
            snapshot.jobs = manager.jobs().map(DownloadView::from_job).collect();
            publish(&sender, &ctx, snapshot.clone());
            let result = manager
                .execute_controlled_with_updates(id, &executor, cancellation, |_, job| {
                    if let Some(view) = snapshot.jobs.iter_mut().find(|view| view.id == job.id()) {
                        *view = DownloadView::from_job(job);
                        publish(&sender, &ctx, snapshot.clone());
                    }
                })
                .await;
            if let Ok(mut slot) = active.lock() {
                *slot = None;
            }
            snapshot.jobs = manager.jobs().map(DownloadView::from_job).collect();
            match result {
                Err(DownloadManagerError::CompletionNotPersisted { destination, .. }) => {
                    if let Some(view) = snapshot.jobs.iter_mut().find(|view| view.id == id) {
                        view.output = Some(destination.final_path().to_owned());
                    }
                    snapshot.notice = Some("The file was saved, but its status may be out of date. Check the destination before trying again.".into());
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
        publish(&sender, &ctx, snapshot.clone());
    }
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

    async fn wait_for(backend: &mut Backend, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        timeout(Duration::from_secs(10), async {
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
            timeout(Duration::from_secs(10), async {
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
    async fn rejected_download_has_a_safe_failure_message() {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let (mut url, server) =
            serve(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
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
        assert!(view.error.is_some());
        assert!(!view.source.contains("private"));
        assert!(view.output.is_none());
        server.await.unwrap();
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
            timeout(Duration::from_secs(10), async {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                received.send(()).unwrap();
                released.await.unwrap();
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                    .await;
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
        timeout(Duration::from_secs(10), request_received)
            .await
            .unwrap()
            .unwrap();
        let snapshot = wait_for(&mut backend, |snapshot| snapshot.active.is_some()).await;
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
