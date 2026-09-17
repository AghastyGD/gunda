use std::time::Duration;

use gunda_core::application::{DownloadCancellation, DownloadEvent, DownloadManager};
use gunda_core::download::{
    DownloadDestination, DownloadOrigin, DownloadState, FileConflictPolicy, NewDownload,
    RequestContext,
};
use gunda_http::{HttpClient, HttpExecutor, partial_path};
use gunda_storage::SqliteDownloadRepository;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use url::Url;

#[derive(Clone, Copy)]
enum CancellationPoint {
    BeforeHeaders,
    DuringBody,
}

struct StalledServer {
    url: Url,
    request_received: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

async fn serve_stalled(point: CancellationPoint) -> StalledServer {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("local listener must bind");

    let address = listener.local_addr().expect("address must exist");

    let (request_seen, request_received) = oneshot::channel();
    let (release, released) = oneshot::channel();

    let task = tokio::spawn(async move {
        timeout(Duration::from_secs(15), async move {
            let (mut socket, _) = listener.accept().await.expect("client must connect");

            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];

            loop {
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("request must be readable");

                assert_ne!(read, 0, "client closed before sending headers");

                request.extend_from_slice(&buffer[..read]);
                assert!(request.len() <= 16 * 1024);

                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }

            assert!(request.starts_with(b"GET /file.bin HTTP/1.1\r\n"));

            request_seen
                .send(())
                .expect("test must be waiting for the request");

            if matches!(point, CancellationPoint::DuringBody) {
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\n\
                          Content-Length: 10\r\n\
                          Content-Type: application/octet-stream\r\n\
                          Connection: close\r\n\
                          \r\n\
                          hello",
                    )
                    .await
                    .expect("partial response must be written");
            }

            // Keep the connection open until cancellation has completed.
            released.await.expect("test must release the server");

            drop(socket);
        })
        .await
        .expect("server must finish within its deadline");
    });

    StalledServer {
        url: Url::parse(&format!("http://{address}/file.bin")).expect("local URL must be valid"),
        request_received,
        release,
        task,
    }
}

async fn run_cancellation_case(point: CancellationPoint) {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let output_directory = directory.path().join("downloads");

    tokio::fs::create_dir(&output_directory)
        .await
        .expect("output directory must be created");

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must open");

    let mut manager = DownloadManager::start(repository)
        .await
        .expect("manager must start");

    let StalledServer {
        url,
        request_received,
        release,
        task,
    } = serve_stalled(point).await;

    let download = NewDownload::new(
        RequestContext::new(url, Vec::new()),
        DownloadDestination::new(
            output_directory.clone(),
            Some("file.bin".to_owned()),
            FileConflictPolicy::Fail,
        ),
        DownloadOrigin::Desktop,
    );

    let created = manager.create(download).await.expect("job must be created");

    let id = created.download_id();
    let staging_path = partial_path(&output_directory, id);
    let final_path = output_directory.join("file.bin");

    let executor = HttpExecutor::new(HttpClient::new().expect("HTTP client must build"));

    let cancellation = DownloadCancellation::new();
    let inspection_handle = cancellation.clone();
    let progress_handle = cancellation.clone();

    let mut observed_progress = Vec::new();

    let report = timeout(Duration::from_secs(8), async {
        let execution = manager.execute_controlled(id, &executor, cancellation, |event| {
            let DownloadEvent::ProgressChanged {
                id: event_id,
                progress,
            } = event
            else {
                panic!("observer must receive progress");
            };

            assert_eq!(event_id, id);
            observed_progress.push(progress.downloaded_bytes());

            if matches!(point, CancellationPoint::DuringBody) && progress.downloaded_bytes() == 5 {
                progress_handle.request();
            }
        });

        let observe_request = async {
            request_received
                .await
                .expect("server must receive the request");

            if matches!(point, CancellationPoint::BeforeHeaders) {
                inspection_handle.request();
            }
        };

        let (result, ()) = tokio::join!(execution, observe_request);

        result.expect("cancellation must persist")
    })
    .await
    .expect("cancellation must finish without waiting for the server");

    let previous_state = match point {
        CancellationPoint::BeforeHeaders => DownloadState::Inspecting,
        CancellationPoint::DuringBody => DownloadState::Downloading,
    };

    assert!(matches!(
        report.event,
        DownloadEvent::StateChanged {
            id: event_id,
            previous,
            current: DownloadState::Cancelled,
        } if event_id == id && previous == previous_state
    ));

    assert!(!report.cleanup_pending);
    assert!(!final_path.exists());

    release
        .send(())
        .expect("server must still be waiting after cancellation");

    task.await.expect("server task must succeed");

    let expected = manager.job(id).expect("job must exist").clone();

    assert_eq!(expected.state(), DownloadState::Cancelled);
    assert!(expected.last_failure().is_none());

    match point {
        CancellationPoint::BeforeHeaders => {
            assert!(observed_progress.is_empty());
            assert_eq!(expected.progress().downloaded_bytes(), 0);
            assert_eq!(expected.progress().total_bytes(), None);
            assert!(expected.resource().is_none());
            assert!(expected.resolved_destination().is_none());
            assert!(!staging_path.exists());
        }

        CancellationPoint::DuringBody => {
            assert_eq!(observed_progress, vec![5]);
            assert_eq!(expected.progress().downloaded_bytes(), 5);
            assert_eq!(expected.progress().total_bytes(), Some(10));

            assert_eq!(
                expected
                    .resource()
                    .expect("resource metadata must exist")
                    .content_type(),
                Some("application/octet-stream"),
            );

            assert_eq!(
                expected
                    .resolved_destination()
                    .expect("planned destination must exist")
                    .final_path(),
                final_path.as_path(),
            );

            assert_eq!(
                tokio::fs::read(&staging_path)
                    .await
                    .expect("partial must remain readable"),
                b"hello",
            );
        }
    }

    manager.into_repository().close().await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");

    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must restart");

    assert!(manager.job(id) == Some(&expected));
    assert!(!final_path.exists());

    match point {
        CancellationPoint::BeforeHeaders => {
            assert!(!staging_path.exists());
        }

        CancellationPoint::DuringBody => {
            assert_eq!(
                tokio::fs::read(&staging_path)
                    .await
                    .expect("partial must survive repository reopen"),
                b"hello",
            );
        }
    }

    manager.into_repository().close().await;
}

#[tokio::test]
async fn cancellation_during_inspection_survives_repository_reopen() {
    run_cancellation_case(CancellationPoint::BeforeHeaders).await;
}

#[tokio::test]
async fn cancelled_partial_download_survives_repository_reopen() {
    run_cancellation_case(CancellationPoint::DuringBody).await;
}
