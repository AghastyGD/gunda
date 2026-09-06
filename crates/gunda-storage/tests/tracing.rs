use std::future::Future;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gunda_core::application::{
    DownloadManager, DownloadRepository, RepositoryError, RepositoryErrorKind,
};
use gunda_core::download::{
    DownloadDestination, DownloadId, DownloadJob, DownloadOrigin, FileConflictPolicy,
    HeaderSensitivity, NewDownload, RequestContext, RequestHeader,
};
use gunda_storage::SqliteDownloadRepository;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, SqliteConnection};
use tempfile::tempdir;
use time::OffsetDateTime;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::fmt::format::FmtSpan;
use url::Url;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("capture lock must not be poisoned")
            .extend_from_slice(bytes);

        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn capture_logs(work: impl Future<Output = ()>) -> String {
    let capture = Capture::default();
    let writer = capture.clone();

    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_target(true)
        .with_max_level(tracing::Level::TRACE)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_writer(move || writer.clone())
        .finish();

    work.with_subscriber(subscriber).await;

    let bytes = capture
        .0
        .lock()
        .expect("capture lock must not be poisoned")
        .clone();

    String::from_utf8(bytes).expect("tracing output must be UTF-8")
}

fn request(origin: DownloadOrigin, sensitive: bool) -> NewDownload {
    let mut headers = vec![RequestHeader::new(
        "User-Agent",
        "private-public-header-marker",
        HeaderSensitivity::Public,
    )];

    if sensitive {
        headers.push(RequestHeader::new(
            "Authorization",
            "Bearer private-auth-marker",
            HeaderSensitivity::Sensitive,
        ));
        headers.push(RequestHeader::new(
            "Cookie",
            "session=private-cookie-marker",
            HeaderSensitivity::Sensitive,
        ));
    }

    NewDownload::new(
        RequestContext::new(
            Url::parse("https://example.invalid/private-url-marker?token=private-query-marker")
                .expect("test URL must be valid"),
            headers,
        ),
        DownloadDestination::new(
            PathBuf::from("private-directory-marker"),
            Some("private-filename-marker.bin".to_owned()),
            FileConflictPolicy::Rename,
        ),
        origin,
    )
}

fn assert_no_private_data(output: &str) {
    for marker in [
        "private-url-marker",
        "private-query-marker",
        "private-public-header-marker",
        "private-auth-marker",
        "private-cookie-marker",
        "private-directory-marker",
        "private-filename-marker",
        "private-database-marker",
        "private-page-marker",
        "private-title-marker",
        "private-error-marker",
    ] {
        assert!(
            !output.contains(marker),
            "private data must not appear in tracing output"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_operations_emit_context_without_request_data() {
    let output = capture_logs(async {
        let directory = tempdir().expect("temporary directory must exist");
        let path = directory.path().join("private-database-marker.sqlite3");

        let repository = SqliteDownloadRepository::open(&path)
            .await
            .expect("repository must open");
        let mut manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        let event = manager
            .create(request(DownloadOrigin::Desktop, false))
            .await
            .expect("creation must succeed");

        let repository = manager.into_repository();

        assert!(
            repository
                .find_by_id(event.download_id())
                .await
                .expect("lookup must succeed")
                .is_some()
        );

        assert!(
            repository
                .find_by_id(DownloadId::new(999).expect("ID must be valid"))
                .await
                .expect("lookup must succeed")
                .is_none()
        );

        repository.close().await;

        let repository = SqliteDownloadRepository::open(&path)
            .await
            .expect("repository must reopen");
        let manager = DownloadManager::start(repository)
            .await
            .expect("manager must restart");

        assert_eq!(manager.len(), 1);
        manager.into_repository().close().await;
    })
    .await;

    for expected in [
        "manager.start",
        "manager.create",
        "storage.open",
        "storage.connect",
        "storage.migrate",
        "storage.create",
        "storage.list",
        "storage.find_by_id",
        "storage.close",
        "download registered",
        "download committed",
        "download_id=1",
        "jobs_loaded=1",
        "found=false",
        "state=Queued",
    ] {
        assert!(
            output.contains(expected),
            "expected safe diagnostic context is missing: {expected}"
        );
    }

    assert_no_private_data(&output);
    assert!(!output.contains("sqlx::query"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_requests_log_categories_without_credentials_or_browser_context() {
    let output = capture_logs(async {
        let repository = SqliteDownloadRepository::open_in_memory()
            .await
            .expect("repository must open");
        let mut manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        let browser = DownloadOrigin::Browser {
            page_url: Url::parse("https://example.invalid/private-page-marker")
                .expect("page URL must be valid"),
            page_title: Some("private-title-marker".to_owned()),
        };

        for input in [
            request(DownloadOrigin::Desktop, true),
            request(browser, false),
        ] {
            let error = match manager.create(input).await {
                Ok(_) => panic!("request must be rejected"),
                Err(error) => error,
            };

            assert_eq!(error.kind(), RepositoryErrorKind::SensitiveDataUnsupported);
        }

        assert!(manager.is_empty());
        manager.into_repository().close().await;
    })
    .await;

    assert!(output.contains("storage.open_in_memory"));
    assert!(output.contains("error_kind=SensitiveDataUnsupported"));
    assert!(output.contains("manager operation failed"));
    assert!(output.contains("repository operation failed"));

    assert!(!output.contains("download registered"));
    assert!(!output.contains("download committed"));

    assert_no_private_data(&output);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn database_open_and_migration_failures_hide_database_paths() {
    let output = capture_logs(async {
        let directory = tempdir().expect("temporary directory must exist");

        let missing_parent = directory
            .path()
            .join("missing")
            .join("private-database-marker.sqlite3");

        let error = match SqliteDownloadRepository::open(&missing_parent).await {
            Ok(_) => panic!("missing parent must prevent database opening"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), RepositoryErrorKind::Unavailable);

        let incompatible_database = directory.path().join("private-database-marker.sqlite3");

        let options = SqliteConnectOptions::new()
            .filename(&incompatible_database)
            .create_if_missing(true)
            .disable_statement_logging();

        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .expect("fixture database must open");

        sqlx::query("CREATE TABLE downloads (id INTEGER PRIMARY KEY)")
            .execute(&mut connection)
            .await
            .expect("fixture table must exist");

        connection
            .close()
            .await
            .expect("fixture connection must close");

        let error = match SqliteDownloadRepository::open(&incompatible_database).await {
            Ok(_) => panic!("incompatible schema must prevent migrations"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), RepositoryErrorKind::Internal);
    })
    .await;

    assert!(output.contains("storage.open"));
    assert!(output.contains("storage.migrate"));
    assert!(output.contains("error_kind=Unavailable"));
    assert!(output.contains("error_kind=Internal"));
    assert!(output.contains("repository operation failed"));

    assert_no_private_data(&output);
}

struct FailingRepository {
    fail_startup: bool,
}

fn untrusted_error() -> RepositoryError {
    // Deliberately violates the adapter's safe-message contract.
    RepositoryError::new(RepositoryErrorKind::Unavailable, "private-error-marker")
}

impl DownloadRepository for FailingRepository {
    async fn create(
        &self,
        _download: NewDownload,
        _created_at: OffsetDateTime,
    ) -> Result<DownloadJob, RepositoryError> {
        Err(untrusted_error())
    }

    async fn find_by_id(&self, _id: DownloadId) -> Result<Option<DownloadJob>, RepositoryError> {
        Ok(None)
    }

    async fn list(&self) -> Result<Vec<DownloadJob>, RepositoryError> {
        if self.fail_startup {
            Err(untrusted_error())
        } else {
            Ok(Vec::new())
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manager_logs_only_the_category_of_repository_errors() {
    let output = capture_logs(async {
        assert!(
            DownloadManager::start(FailingRepository { fail_startup: true })
                .await
                .is_err()
        );

        let mut manager = DownloadManager::start(FailingRepository {
            fail_startup: false,
        })
        .await
        .expect("manager must start");

        assert!(
            manager
                .create(request(DownloadOrigin::Desktop, false))
                .await
                .is_err()
        );

        assert!(manager.is_empty());
    })
    .await;

    assert!(output.contains("manager.start"));
    assert!(output.contains("manager.create"));
    assert!(output.contains("error_kind=Unavailable"));
    assert!(output.contains("manager operation failed"));

    assert_no_private_data(&output);
}
