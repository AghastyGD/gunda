use std::path::Path;
use std::time::Duration;

use gunda_core::application::{DownloadEvent, DownloadManager, DownloadManagerError};
use gunda_core::download::{
    DownloadDestination, DownloadOrigin, DownloadState, FileConflictPolicy, NewDownload,
    RequestContext,
};
use gunda_http::{HttpClient, HttpExecutor, partial_path};
use gunda_storage::SqliteDownloadRepository;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, SqliteConnection};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use url::Url;

async fn serve_file() -> (Url, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("local listener must bind");

    let address = listener.local_addr().expect("address must exist");

    let task = tokio::spawn(async move {
        timeout(Duration::from_secs(10), async move {
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

            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Length: 5\r\n\
                      Content-Type: application/octet-stream\r\n\
                      Connection: close\r\n\
                      \r\n\
                      hello",
                )
                .await
                .expect("response must be written");

            socket.shutdown().await.expect("socket must close");
        })
        .await
        .expect("server must finish within its deadline");
    });

    let url = Url::parse(&format!("http://{address}/file.bin")).expect("local URL must be valid");

    (url, task)
}

fn new_download(url: Url, directory: &Path) -> NewDownload {
    NewDownload::new(
        RequestContext::new(url, Vec::new()),
        DownloadDestination::new(
            directory.to_path_buf(),
            Some("file.bin".to_owned()),
            FileConflictPolicy::Fail,
        ),
        DownloadOrigin::Desktop,
    )
}

async fn reject_completion(database_path: &Path) {
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .disable_statement_logging();

    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .expect("test connection must open");

    sqlx::query(
        r#"
        CREATE TRIGGER reject_completion
        BEFORE UPDATE ON downloads
        WHEN NEW.state = 'completed'
        BEGIN
            SELECT RAISE(ABORT, 'test completion rejection');
        END
        "#,
    )
    .execute(&mut connection)
    .await
    .expect("failure injection trigger must be created");

    connection
        .close()
        .await
        .expect("test connection must close");
}

async fn run_persistent_execution(reject_completed: bool) {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let output_directory = directory.path().join("downloads");

    tokio::fs::create_dir(&output_directory)
        .await
        .expect("output directory must be created");

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must open");

    require_planned_destination(&database_path).await;

    if reject_completed {
        reject_completion(&database_path).await;
    }

    let mut manager = DownloadManager::start(repository)
        .await
        .expect("manager must start");

    let (url, server) = serve_file().await;

    let created = manager
        .create(new_download(url, &output_directory))
        .await
        .expect("job must be created");

    let id = created.download_id();
    let final_path = output_directory.join("file.bin");

    assert_eq!(
        manager.job(id).expect("job must exist").state(),
        DownloadState::Queued,
    );

    let executor = HttpExecutor::new(HttpClient::new().expect("HTTP client must build"));

    let result = manager.execute(id, &executor).await;

    if reject_completed {
        let error = match result {
            Ok(_) => panic!("completion persistence must fail"),
            Err(error) => error,
        };

        let DownloadManagerError::CompletionNotPersisted {
            id: failed_id,
            destination,
            written_bytes,
            cleanup_pending,
            ..
        } = error
        else {
            panic!("published output must be represented in the error");
        };

        assert_eq!(failed_id, id);
        assert_eq!(destination.final_path(), final_path.as_path());
        assert_eq!(written_bytes, 5);
        assert!(!cleanup_pending);
    } else {
        let report = result.expect("execution must succeed");

        let DownloadEvent::Completed {
            id: completed_id,
            destination,
        } = report.event
        else {
            panic!("execution must return Completed");
        };

        assert_eq!(completed_id, id);
        assert_eq!(destination.final_path(), final_path.as_path());
        assert!(!report.cleanup_pending);
    }

    server.await.expect("server task must succeed");

    assert_eq!(
        tokio::fs::read(&final_path)
            .await
            .expect("published output must be readable"),
        b"hello",
    );

    assert!(!partial_path(&output_directory, id).exists());

    let expected = manager.job(id).expect("job must exist").clone();

    let expected_state = if reject_completed {
        DownloadState::Finalizing
    } else {
        DownloadState::Completed
    };

    assert_eq!(expected.state(), expected_state);
    assert_eq!(expected.progress().downloaded_bytes(), 5);
    assert_eq!(expected.progress().total_bytes(), Some(5));

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
            .expect("selected destination must exist")
            .final_path(),
        final_path.as_path(),
    );

    manager.into_repository().close().await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");

    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must restart");

    assert!(manager.job(id) == Some(&expected));

    assert_eq!(
        tokio::fs::read(&final_path)
            .await
            .expect("output must survive repository reopen"),
        b"hello",
    );

    manager.into_repository().close().await;
}

async fn require_planned_destination(database_path: &Path) {
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .disable_statement_logging();

    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .expect("test connection must open");

    sqlx::query(
        r#"
        CREATE TRIGGER require_planned_destination
        BEFORE UPDATE ON downloads
        WHEN NEW.state = 'downloading'
             AND NEW.resolved_destination_path IS NULL
        BEGIN
            SELECT RAISE(ABORT, 'planned destination is required');
        END
        "#,
    )
    .execute(&mut connection)
    .await
    .expect("destination assertion trigger must be created");

    connection
        .close()
        .await
        .expect("test connection must close");
}

#[tokio::test]
async fn completed_download_survives_repository_reopen() {
    run_persistent_execution(false).await;
}

#[tokio::test]
async fn published_file_survives_completion_persistence_failure() {
    run_persistent_execution(true).await;
}
