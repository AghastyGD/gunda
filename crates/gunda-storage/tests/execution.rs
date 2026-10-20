use std::path::Path;
use std::time::Duration;

use gunda_core::application::{
    DownloadCancellation, DownloadEvent, DownloadManager, DownloadManagerError, DownloadRepository,
};
use gunda_core::download::{
    DownloadDestination, DownloadOrigin, DownloadProgress, DownloadState, FailureKind,
    FileConflictPolicy, NewDownload, RequestContext, ResolvedDestination, ResourceDescriptor,
    ResourceKind,
};
use gunda_http::{
    HttpClient, HttpExecutor, HttpResumeState, HttpResumeStore, StrongEntityTag, partial_path,
};
use gunda_storage::SqliteDownloadRepository;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, SqliteConnection};
use tempfile::tempdir;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use url::Url;

async fn serve_file() -> (Url, JoinHandle<()>) {
    serve_response(
        b"HTTP/1.1 200 OK\r\n\
          Content-Length: 5\r\n\
          Content-Type: application/octet-stream\r\n\
          ETag: \"version-31\"\r\n\
          Connection: close\r\n\
          \r\n\
          hello",
    )
    .await
}

async fn serve_response(response: &'static [u8]) -> (Url, JoinHandle<()>) {
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
                .write_all(response)
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

async fn serve_range_response() -> (Url, JoinHandle<Vec<u8>>) {
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

            socket
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\n\
                      Content-Range: bytes 5-9/10\r\n\
                      Content-Length: 5\r\n\
                      ETag: \"version-31\"\r\n\
                      Connection: close\r\n\r\n\
                      world",
                )
                .await
                .expect("response must be written");
            socket.shutdown().await.expect("socket must close");
            request
        })
        .await
        .expect("server must finish within its deadline")
    });

    let url = Url::parse(&format!("http://{address}/file.bin")).expect("local URL must be valid");
    (url, task)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
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
            return request;
        }
    }
}

async fn serve_captured_responses(
    responses: Vec<&'static [u8]>,
) -> (Url, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("local listener must bind");
    let address = listener.local_addr().expect("address must exist");

    let task = tokio::spawn(async move {
        timeout(Duration::from_secs(15), async move {
            let mut requests = Vec::with_capacity(responses.len());
            for response in responses {
                let (mut socket, _) = listener.accept().await.expect("client must connect");
                requests.push(read_request(&mut socket).await);
                socket
                    .write_all(response)
                    .await
                    .expect("response must be written");
                socket.shutdown().await.expect("socket must close");
            }
            requests
        })
        .await
        .expect("server must finish within its deadline")
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

async fn persist_interrupted_fixture(
    database_path: &Path,
    output_directory: &Path,
    url: Url,
    generic_bytes: u64,
    durable_bytes: u64,
    partial: &[u8],
) -> gunda_core::download::DownloadId {
    persist_interrupted_fixture_with_total(
        database_path,
        output_directory,
        url,
        generic_bytes,
        durable_bytes,
        partial,
        10,
    )
    .await
}

async fn persist_interrupted_fixture_with_total(
    database_path: &Path,
    output_directory: &Path,
    url: Url,
    generic_bytes: u64,
    durable_bytes: u64,
    partial: &[u8],
    total_bytes: u64,
) -> gunda_core::download::DownloadId {
    let repository = SqliteDownloadRepository::open(database_path)
        .await
        .expect("repository must open");
    let now = OffsetDateTime::now_utc();
    let mut job = repository
        .create(new_download(url, output_directory), now)
        .await
        .expect("job must be created");
    let id = job.id();

    job.transition_to(DownloadState::Inspecting, now)
        .expect("inspection must start");
    job.set_resource(
        ResourceDescriptor::new(
            ResourceKind::File,
            Some("file.bin".to_owned()),
            Some("application/octet-stream".to_owned()),
        ),
        now,
    );
    job.resolve_destination(
        ResolvedDestination::new(output_directory.join("file.bin")),
        now,
    );
    job.update_progress(
        DownloadProgress::new(generic_bytes, Some(total_bytes)).expect("progress must be valid"),
        now,
    );
    job.transition_to(DownloadState::Downloading, now)
        .expect("download must start");
    repository.save(&job).await.expect("job must be saved");

    let store = repository.http_resume_store();
    let tag = StrongEntityTag::parse(b"\"version-31\"").expect("tag must be valid");
    store
        .initialize(id, &tag, total_bytes)
        .await
        .expect("resume state must initialize");
    store
        .save_checkpoint(
            id,
            &HttpResumeState::new(tag, total_bytes, durable_bytes)
                .expect("checkpoint must be valid"),
        )
        .await
        .expect("checkpoint must be saved");
    tokio::fs::write(partial_path(output_directory, id), partial)
        .await
        .expect("partial fixture must be written");
    repository.close().await;

    id
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

    let resume_store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
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

    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("HTTP client must build"),
        resume_store.clone(),
    );

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

    let expected_resume = HttpResumeState::new(
        StrongEntityTag::parse(b"\"version-31\"").expect("tag must be valid"),
        5,
        5,
    )
    .expect("checkpoint must be valid");
    assert_eq!(
        resume_store.find(id).await.expect("checkpoint must load"),
        Some(expected_resume.clone())
    );

    let expected = manager.job(id).expect("job must exist");

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

    assert_eq!(
        repository
            .http_resume_store()
            .find(id)
            .await
            .expect("checkpoint must survive reopen"),
        Some(expected_resume),
    );

    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must restart");

    let restored = manager.job(id).expect("restored job must exist");

    if reject_completed {
        let mut recovered = expected.snapshot();
        recovered.state = DownloadState::Interrupted;
        recovered.updated_at = restored.updated_at();

        assert!(restored.snapshot() == recovered);
        assert!(restored.updated_at() >= expected.updated_at());
    } else {
        assert!(restored == expected);
    }

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

#[tokio::test]
async fn checkpoint_failure_keeps_partial_and_prevents_publication() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let output_directory = directory.path().join("downloads");
    tokio::fs::create_dir(&output_directory)
        .await
        .expect("output directory must be created");

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must open");
    let resume_store = repository.http_resume_store();
    let options = SqliteConnectOptions::new()
        .filename(&database_path)
        .disable_statement_logging();
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .expect("test connection must open");
    sqlx::query(
        r#"
        CREATE TRIGGER reject_checkpoint
        BEFORE UPDATE ON http_resume_state
        BEGIN
            SELECT RAISE(ABORT, 'test checkpoint rejection');
        END
        "#,
    )
    .execute(&mut connection)
    .await
    .expect("failure trigger must be created");
    connection
        .close()
        .await
        .expect("test connection must close");

    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must start");
    let (url, server) = serve_file().await;
    let id = manager
        .create(new_download(url, &output_directory))
        .await
        .expect("job must be created")
        .download_id();
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("HTTP client must build"),
        resume_store.clone(),
    );
    let report = timeout(Duration::from_secs(10), manager.execute(id, &executor))
        .await
        .expect("execution must not stall")
        .expect("failure must persist");

    assert!(matches!(report.event, DownloadEvent::Failed { .. }));
    let job = manager.job(id).expect("job must exist");
    assert_eq!(job.state(), DownloadState::Failed);
    assert_eq!(
        job.last_failure().expect("failure must exist").kind(),
        FailureKind::Storage
    );
    assert_eq!(
        resume_store
            .find(id)
            .await
            .expect("checkpoint must load")
            .expect("initial checkpoint must exist")
            .durable_bytes(),
        0
    );
    assert_eq!(
        tokio::fs::read(partial_path(&output_directory, id))
            .await
            .expect("partial must remain"),
        b"hello"
    );
    assert!(!output_directory.join("file.bin").exists());

    server.await.expect("server must finish");
    manager.into_repository().close().await;
}

#[tokio::test]
async fn truncated_response_checkpoints_only_the_written_prefix() {
    let directory = tempdir().expect("temporary directory must exist");
    let repository = SqliteDownloadRepository::open_in_memory()
        .await
        .expect("repository must open");
    let store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must start");
    let (url, server) = serve_response(
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nETag: \"version-31\"\r\nConnection: close\r\n\r\nhello",
    ).await;
    let id = manager
        .create(new_download(url, directory.path()))
        .await
        .expect("job must be created")
        .download_id();
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("client must build"),
        store.clone(),
    );
    let report = timeout(Duration::from_secs(10), manager.execute(id, &executor))
        .await
        .expect("execution must not stall")
        .expect("failure must persist");
    assert!(matches!(report.event, DownloadEvent::Failed { .. }));
    let state = store
        .find(id)
        .await
        .expect("checkpoint must load")
        .expect("checkpoint must exist");
    let partial = tokio::fs::read(partial_path(directory.path(), id))
        .await
        .expect("partial must remain");
    assert_eq!(partial, b"hello");
    assert_eq!(state.total_bytes(), 10);
    assert_eq!(state.durable_bytes(), 5);
    assert_eq!(
        manager
            .job(id)
            .expect("job must exist")
            .progress()
            .downloaded_bytes(),
        5
    );
    assert!(!directory.path().join("file.bin").exists());
    server.await.expect("server must finish");
    manager.into_repository().close().await;
}

#[tokio::test]
async fn full_downloads_without_a_strong_tag_and_known_size_have_no_resume_state() {
    for response in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: W/\"version-31\"\r\nConnection: close\r\n\r\nhello".as_slice(),
        b"HTTP/1.1 200 OK\r\nETag: \"version-31\"\r\nConnection: close\r\n\r\nhello".as_slice(),
    ] {
        let directory = tempdir().expect("temporary directory must exist");
        let repository = SqliteDownloadRepository::open_in_memory().await.expect("repository must open");
        let store = repository.http_resume_store();
        let manager = DownloadManager::start(repository).await.expect("manager must start");
        let (url, server) = serve_response(response).await;
        let id = manager.create(new_download(url, directory.path())).await.expect("job must be created").download_id();
        let executor = HttpExecutor::with_resume_store(HttpClient::new().expect("client must build"), store.clone());
        let report = timeout(Duration::from_secs(10), manager.execute(id, &executor)).await.expect("execution must not stall").expect("execution must succeed");
        assert!(matches!(report.event, DownloadEvent::Completed { .. }));
        assert!(store.find(id).await.expect("lookup must succeed").is_none());
        assert_eq!(tokio::fs::read(directory.path().join("file.bin")).await.expect("output must exist"), b"hello");
        server.await.expect("server must finish");
        manager.into_repository().close().await;
    }
}

#[tokio::test]
async fn existing_resume_state_cannot_be_relabelled_by_a_fresh_transfer() {
    let directory = tempdir().expect("temporary directory must exist");
    let repository = SqliteDownloadRepository::open_in_memory()
        .await
        .expect("repository must open");
    let store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must start");
    let url = Url::parse("http://127.0.0.1:9/file.bin").expect("test URL must be valid");
    let id = manager
        .create(new_download(url, directory.path()))
        .await
        .expect("job must be created")
        .download_id();
    let original = store
        .initialize(
            id,
            &StrongEntityTag::parse(b"\"original\"").expect("tag must be valid"),
            10,
        )
        .await
        .expect("state must initialize");
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("client must build"),
        store.clone(),
    );
    let report = timeout(Duration::from_secs(10), manager.execute(id, &executor))
        .await
        .expect("execution must not stall")
        .expect("failure must persist");
    assert!(matches!(report.event, DownloadEvent::Failed { .. }));
    assert_eq!(
        store.find(id).await.expect("checkpoint must load"),
        Some(original)
    );
    assert!(!partial_path(directory.path(), id).exists());
    assert!(!directory.path().join("file.bin").exists());
    manager.into_repository().close().await;
}

#[tokio::test]
async fn interrupted_download_resumes_from_the_durable_checkpoint() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let output_directory = directory.path().join("downloads");
    tokio::fs::create_dir(&output_directory)
        .await
        .expect("output directory must be created");

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must open");
    let resume_store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must start");
    let (url, server) = serve_range_response().await;
    let id = manager
        .create(new_download(url, &output_directory))
        .await
        .expect("job must be created")
        .download_id();

    let mut job = manager.job(id).expect("job must exist");
    let now = OffsetDateTime::now_utc();
    job.transition_to(DownloadState::Inspecting, now)
        .expect("inspection must start");
    job.set_resource(
        ResourceDescriptor::new(
            ResourceKind::File,
            Some("file.bin".to_owned()),
            Some("application/octet-stream".to_owned()),
        ),
        now,
    );
    job.resolve_destination(
        ResolvedDestination::new(output_directory.join("file.bin")),
        now,
    );
    job.update_progress(
        DownloadProgress::new(8, Some(10)).expect("progress must be valid"),
        now,
    );
    job.transition_to(DownloadState::Downloading, now)
        .expect("download must start");

    let repository = manager.into_repository();
    repository.save(&job).await.expect("job must be saved");
    let initial = resume_store
        .initialize(
            id,
            &StrongEntityTag::parse(b"\"version-31\"").expect("tag must be valid"),
            10,
        )
        .await
        .expect("resume state must initialize");
    let checkpoint = HttpResumeState::new(initial.strong_etag().clone(), 10, 5)
        .expect("checkpoint must be valid");
    resume_store
        .save_checkpoint(id, &checkpoint)
        .await
        .expect("checkpoint must be saved");
    tokio::fs::write(partial_path(&output_directory, id), b"helloXYZ")
        .await
        .expect("partial fixture must be written");
    repository.close().await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");
    let resume_store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must recover");
    assert_eq!(
        manager.job(id).expect("job must exist").state(),
        DownloadState::Interrupted
    );
    manager.resume(id).await.expect("resume must queue the job");

    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("client must build"),
        resume_store.clone(),
    );
    let report = manager
        .execute(id, &executor)
        .await
        .expect("resumed execution must succeed");
    assert!(matches!(report.event, DownloadEvent::Completed { .. }));
    assert_eq!(
        tokio::fs::read(output_directory.join("file.bin"))
            .await
            .expect("output must be readable"),
        b"helloworld"
    );
    assert_eq!(
        resume_store
            .find(id)
            .await
            .expect("checkpoint must load")
            .expect("checkpoint must exist")
            .durable_bytes(),
        10
    );

    let request = server.await.expect("server must succeed");
    let request = String::from_utf8(request)
        .expect("request must be UTF-8")
        .to_ascii_lowercase();
    assert!(request.contains("range: bytes=5-\r\n"));
    assert!(request.contains("if-range: \"version-31\"\r\n"));
    manager.into_repository().close().await;
}

#[tokio::test]
async fn changed_resource_rejects_resume_without_touching_the_partial() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let output_directory = directory.path().join("downloads");
    tokio::fs::create_dir(&output_directory)
        .await
        .expect("output directory must be created");
    let (url, server) = serve_captured_responses(vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nETag: \"version-32\"\r\nConnection: close\r\n\r\nchanged!!!",
    ])
    .await;
    let id =
        persist_interrupted_fixture(&database_path, &output_directory, url, 5, 5, b"hello").await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");
    let store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must recover");
    manager.resume(id).await.expect("resume must queue the job");
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("HTTP client must build"),
        store.clone(),
    );

    let report = manager
        .execute(id, &executor)
        .await
        .expect("recovery failure must persist");

    assert!(matches!(report.event, DownloadEvent::Failed { .. }));
    let job = manager.job(id).expect("job must exist");
    let failure = job.last_failure().expect("failure must exist");
    assert_eq!(failure.kind(), FailureKind::Integrity);
    assert!(!failure.is_retryable());
    assert_eq!(
        tokio::fs::read(partial_path(&output_directory, id))
            .await
            .expect("partial must remain"),
        b"hello"
    );
    assert_eq!(
        store
            .find(id)
            .await
            .expect("checkpoint must load")
            .expect("checkpoint must remain")
            .durable_bytes(),
        5
    );
    assert!(!output_directory.join("file.bin").exists());
    let requests = server.await.expect("server must succeed");
    let request = String::from_utf8(requests[0].clone())
        .expect("request must be UTF-8")
        .to_ascii_lowercase();
    assert!(request.contains("range: bytes=5-\r\n"));
    assert!(request.contains("if-range: \"version-31\"\r\n"));
    manager.into_repository().close().await;
}

#[tokio::test]
async fn zero_checkpoint_resumes_with_a_validated_full_response() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let output_directory = directory.path().join("downloads");
    tokio::fs::create_dir(&output_directory)
        .await
        .expect("output directory must be created");
    let (url, server) = serve_captured_responses(vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nETag: \"version-31\"\r\nConnection: close\r\n\r\nhelloworld",
    ])
    .await;
    let id = persist_interrupted_fixture(&database_path, &output_directory, url, 0, 0, b"").await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");
    let store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must recover");
    manager.resume(id).await.expect("resume must queue the job");
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("HTTP client must build"),
        store.clone(),
    );

    let report = manager
        .execute(id, &executor)
        .await
        .expect("zero checkpoint must resume");

    assert!(matches!(report.event, DownloadEvent::Completed { .. }));
    assert_eq!(
        tokio::fs::read(output_directory.join("file.bin"))
            .await
            .expect("output must exist"),
        b"helloworld"
    );
    assert_eq!(
        store
            .find(id)
            .await
            .expect("checkpoint must load")
            .expect("checkpoint must remain")
            .durable_bytes(),
        10
    );
    let requests = server.await.expect("server must succeed");
    let request = String::from_utf8(requests[0].clone())
        .expect("request must be UTF-8")
        .to_ascii_lowercase();
    assert!(!request.contains("range:"));
    assert!(!request.contains("if-range:"));
    manager.into_repository().close().await;
}

#[tokio::test]
async fn cancellation_during_resume_checkpoints_the_new_prefix_without_publication() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let output_directory = directory.path().join("downloads");
    tokio::fs::create_dir(&output_directory)
        .await
        .expect("output directory must be created");
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("local listener must bind");
    let address = listener.local_addr().expect("address must exist");
    let (release, released) = oneshot::channel();
    let server = tokio::spawn(async move {
        timeout(Duration::from_secs(10), async move {
            let (mut socket, _) = listener.accept().await.expect("client must connect");
            let request = read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 5-9/10\r\nContent-Length: 5\r\nETag: \"version-31\"\r\nConnection: close\r\n\r\nwo",
                )
                .await
                .expect("partial response must be written");
            released.await.expect("test must release server");
            drop(socket);
            request
        })
        .await
        .expect("server must finish within its deadline")
    });
    let url = Url::parse(&format!("http://{address}/file.bin")).expect("local URL must be valid");
    let id =
        persist_interrupted_fixture(&database_path, &output_directory, url, 5, 5, b"hello").await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");
    let store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must recover");
    manager.resume(id).await.expect("resume must queue the job");
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("HTTP client must build"),
        store.clone(),
    );
    let cancellation = DownloadCancellation::new();
    let control = cancellation.clone();

    let report = timeout(
        Duration::from_secs(8),
        manager.execute_controlled(id, &executor, cancellation, |event| {
            if let DownloadEvent::ProgressChanged { progress, .. } = event
                && progress.downloaded_bytes() == 7
            {
                control.request();
            }
        }),
    )
    .await
    .expect("cancellation must not wait for EOF")
    .expect("cancellation must persist");

    assert!(matches!(
        report.event,
        DownloadEvent::StateChanged {
            current: DownloadState::Cancelled,
            ..
        }
    ));
    assert_eq!(
        tokio::fs::read(partial_path(&output_directory, id))
            .await
            .expect("partial must remain"),
        b"hellowo"
    );
    assert_eq!(
        store
            .find(id)
            .await
            .expect("checkpoint must load")
            .expect("checkpoint must remain")
            .durable_bytes(),
        7
    );
    assert!(!output_directory.join("file.bin").exists());
    release.send(()).expect("server must still be waiting");
    let request = String::from_utf8(server.await.expect("server must succeed"))
        .expect("request must be UTF-8")
        .to_ascii_lowercase();
    assert!(request.contains("range: bytes=5-\r\n"));
    assert!(request.contains("if-range: \"version-31\"\r\n"));
    manager.into_repository().close().await;
}

#[tokio::test]
async fn download_resumes_after_repeated_process_interruptions() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let output_directory = directory.path().join("downloads");
    tokio::fs::create_dir(&output_directory)
        .await
        .expect("output directory must be created");
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("local listener must bind");
    let address = listener.local_addr().expect("address must exist");
    let (release_first, first_released) = oneshot::channel();
    let checkpoint_prefix = vec![b'a'; 8 * 1024 * 1024 + 1];
    let total_bytes = 16 * 1024 * 1024 + 5;
    let server = tokio::spawn(async move {
        timeout(Duration::from_secs(45), async move {
            let (mut first, _) = listener.accept().await.expect("client must connect");
            let first_request = read_request(&mut first).await;
            let remaining = total_bytes - 5;
            let headers = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 5-{}/{total_bytes}\r\nContent-Length: {remaining}\r\nETag: \"version-31\"\r\nConnection: close\r\n\r\n",
                total_bytes - 1,
            );
            first
                .write_all(headers.as_bytes())
                .await
                .expect("first response headers must be written");
            first
                .write_all(&checkpoint_prefix)
                .await
                .expect("checkpoint prefix must be written");
            first_released.await.expect("test must release first server");
            drop(first);

            let (mut second, _) = listener.accept().await.expect("client must reconnect");
            let second_request = read_request(&mut second).await;
            let second_request_text = String::from_utf8_lossy(&second_request).to_ascii_lowercase();
            let range_line = second_request_text
                .lines()
                .find(|line| line.starts_with("range: bytes="))
                .expect("second request must contain a byte range");
            let start = range_line
                .trim_start_matches("range: bytes=")
                .trim_end_matches('-')
                .parse::<u64>()
                .expect("range offset must be valid");
            let remaining = total_bytes - start;
            let headers = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{total_bytes}\r\nContent-Length: {remaining}\r\nETag: \"version-31\"\r\nConnection: close\r\n\r\n",
                total_bytes - 1,
            );
            second
                .write_all(headers.as_bytes())
                .await
                .expect("remaining response headers must be written");
            second
                .write_all(&vec![b'z'; remaining as usize])
                .await
                .expect("remaining response must be written");
            second.shutdown().await.expect("socket must close");
            (first_request, second_request, start)
        })
        .await
        .expect("server must finish within its deadline")
    });
    let url = Url::parse(&format!("http://{address}/file.bin")).expect("local URL must be valid");
    let id = persist_interrupted_fixture_with_total(
        &database_path,
        &output_directory,
        url,
        5,
        5,
        b"hello",
        total_bytes,
    )
    .await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");
    let store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must recover");
    manager.resume(id).await.expect("resume must queue the job");
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("HTTP client must build"),
        store.clone(),
    );
    let execution = tokio::spawn(async move { manager.execute(id, &executor).await });

    let checkpoint = timeout(Duration::from_secs(45), async {
        loop {
            if let Some(state) = store.find(id).await.expect("checkpoint must load")
                && state.durable_bytes() >= 5 + 8 * 1024 * 1024
            {
                break state.durable_bytes();
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("second durable checkpoint must be reached");

    execution.abort();
    let Err(interrupted) = execution.await else {
        panic!("execution must be interrupted");
    };
    assert!(interrupted.is_cancelled());
    release_first
        .send(())
        .expect("first server must still be waiting");

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen again");
    let store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must recover again");
    assert_eq!(
        manager.job(id).expect("job must exist").state(),
        DownloadState::Interrupted
    );
    manager.resume(id).await.expect("resume must queue again");
    let executor =
        HttpExecutor::with_resume_store(HttpClient::new().expect("HTTP client must build"), store);
    let report = manager
        .execute(id, &executor)
        .await
        .expect("second resume must complete");

    assert!(matches!(report.event, DownloadEvent::Completed { .. }));
    let output = tokio::fs::read(output_directory.join("file.bin"))
        .await
        .expect("output must exist");
    assert_eq!(output.len(), total_bytes as usize);
    assert_eq!(&output[..5], b"hello");
    assert!(
        output[5..checkpoint as usize]
            .iter()
            .all(|byte| *byte == b'a')
    );
    assert!(
        output[checkpoint as usize..]
            .iter()
            .all(|byte| *byte == b'z')
    );
    let (first_request, second_request, resumed_from) = server.await.expect("server must succeed");
    assert_eq!(resumed_from, checkpoint);
    let first = String::from_utf8(first_request)
        .expect("request must be UTF-8")
        .to_ascii_lowercase();
    let second = String::from_utf8(second_request)
        .expect("request must be UTF-8")
        .to_ascii_lowercase();
    assert!(first.contains("range: bytes=5-\r\n"));
    assert!(second.contains(&format!("range: bytes={checkpoint}-\r\n")));
    manager.into_repository().close().await;
}

#[tokio::test]
async fn retryable_failure_resumes_from_the_durable_checkpoint() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let output_directory = directory.path().join("downloads");
    tokio::fs::create_dir(&output_directory)
        .await
        .expect("output directory must be created");
    let (url, server) = serve_range_response().await;
    let id =
        persist_interrupted_fixture(&database_path, &output_directory, url, 5, 5, b"hello").await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");
    let mut job = repository
        .find_by_id(id)
        .await
        .expect("job lookup must succeed")
        .expect("job must exist");
    job.fail(
        gunda_core::download::DownloadFailure::new(FailureKind::Network, "connection closed", true),
        OffsetDateTime::now_utc(),
    )
    .expect("active job must fail");
    repository.save(&job).await.expect("failure must persist");
    repository.close().await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");
    let store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must start");
    assert_eq!(
        manager.job(id).expect("job must exist").state(),
        DownloadState::Failed
    );
    manager.retry(id).await.expect("retry must queue the job");

    let executor =
        HttpExecutor::with_resume_store(HttpClient::new().expect("HTTP client must build"), store);
    let report = manager
        .execute(id, &executor)
        .await
        .expect("retry must execute");

    assert!(matches!(report.event, DownloadEvent::Completed { .. }));
    assert_eq!(
        tokio::fs::read(output_directory.join("file.bin"))
            .await
            .expect("output must be readable"),
        b"helloworld"
    );
    let request = server.await.expect("server must succeed");
    let request = String::from_utf8(request)
        .expect("request must be UTF-8")
        .to_ascii_lowercase();
    assert!(request.contains("range: bytes=5-\r\n"));
    assert!(request.contains("if-range: \"version-31\"\r\n"));
    manager.into_repository().close().await;
}

#[tokio::test]
async fn recovery_rejects_a_partial_shorter_than_the_durable_checkpoint() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let output_directory = directory.path().join("downloads");
    tokio::fs::create_dir(&output_directory)
        .await
        .expect("output directory must be created");
    let url = Url::parse("http://127.0.0.1:9/file.bin").expect("test URL must be valid");
    let id =
        persist_interrupted_fixture(&database_path, &output_directory, url, 5, 5, b"hell").await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");
    let store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must recover");
    manager.resume(id).await.expect("resume must queue the job");
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("client must build"),
        store.clone(),
    );

    let report = manager
        .execute(id, &executor)
        .await
        .expect("recovery failure must be persisted");
    assert!(matches!(report.event, DownloadEvent::Failed { .. }));
    assert_eq!(
        manager
            .job(id)
            .expect("job must exist")
            .last_failure()
            .expect("failure must exist")
            .kind(),
        FailureKind::Integrity
    );
    assert_eq!(
        tokio::fs::read(partial_path(&output_directory, id))
            .await
            .expect("partial must remain"),
        b"hell"
    );
    assert_eq!(
        store
            .find(id)
            .await
            .expect("checkpoint must load")
            .expect("checkpoint must exist")
            .durable_bytes(),
        5
    );
    assert!(!output_directory.join("file.bin").exists());
    manager.into_repository().close().await;
}

#[tokio::test]
async fn complete_checkpoint_finalizes_without_another_request() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let output_directory = directory.path().join("downloads");
    tokio::fs::create_dir(&output_directory)
        .await
        .expect("output directory must be created");
    let url = Url::parse("http://127.0.0.1:9/file.bin").expect("test URL must be valid");
    let id = persist_interrupted_fixture(
        &database_path,
        &output_directory,
        url,
        10,
        10,
        b"helloworld",
    )
    .await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");
    let store = repository.http_resume_store();
    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must recover");
    manager.resume(id).await.expect("resume must queue the job");
    let executor =
        HttpExecutor::with_resume_store(HttpClient::new().expect("client must build"), store);

    let report = manager
        .execute(id, &executor)
        .await
        .expect("complete partial must finalize");
    assert!(matches!(report.event, DownloadEvent::Completed { .. }));
    assert_eq!(
        tokio::fs::read(output_directory.join("file.bin"))
            .await
            .expect("output must exist"),
        b"helloworld"
    );
    assert!(!partial_path(&output_directory, id).exists());
    manager.into_repository().close().await;
}
