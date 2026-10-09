use std::path::{Path, PathBuf};

use gunda_core::application::{DownloadRepository, RepositoryErrorKind};
use gunda_core::download::{
    DownloadId, DownloadState, FailureKind, FileConflictPolicy, HeaderSensitivity, ResourceKind,
};
use gunda_http::{HttpResumeStore, StrongEntityTag};
use gunda_storage::SqliteDownloadRepository;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{ConnectOptions, SqlitePool};
use tempfile::tempdir;

const DOWNLOAD_ID: i64 = 1;

async fn connect(path: &Path, create_if_missing: bool) -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(create_if_missing)
        .foreign_keys(true)
        .disable_statement_logging();

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("test database must open");

    let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&pool)
        .await
        .expect("foreign key setting must be readable");
    assert_eq!(foreign_keys, 1, "foreign keys must be enabled");

    pool
}

async fn apply_first_two_migrations(path: &Path) -> SqlitePool {
    let migration_directory = tempdir().expect("temporary migration directory must exist");
    let migrations = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations");

    for filename in [
        "0001_create_downloads.sql",
        "0002_add_download_execution_metadata.sql",
    ] {
        std::fs::copy(
            migrations.join(filename),
            migration_directory.path().join(filename),
        )
        .expect("migration fixture must be copied");
    }

    let pool = connect(path, true).await;
    let migrator = Migrator::new(migration_directory.path())
        .await
        .expect("old migrator must load");
    migrator
        .run(&pool)
        .await
        .expect("old migrations must apply");

    pool
}

async fn create_current_database(path: &Path) {
    let repository = SqliteDownloadRepository::open(path)
        .await
        .expect("current database must open");
    repository.close().await;
}

async fn insert_download_with_metadata(pool: &SqlitePool) {
    sqlx::query(
        r#"
        INSERT INTO downloads (
            id,
            source_url,
            origin,
            source_page_url,
            source_page_title,
            destination_directory,
            preferred_filename,
            conflict_policy,
            state,
            downloaded_bytes,
            total_bytes,
            created_at_unix_ms,
            updated_at_unix_ms,
            resolved_destination_path,
            resource_kind,
            resource_display_name,
            resource_content_type,
            last_failure_kind,
            last_failure_message,
            last_failure_retryable
        )
        VALUES (
            $1, $2, 'desktop', NULL, NULL, $3, $4, 'fail', 'failed',
            7, 12, 1000, 4000, $5, 'file', $6, $7, 'network', $8, 1
        )
        "#,
    )
    .bind(DOWNLOAD_ID)
    .bind("https://example.com/archive.bin")
    .bind(native_path_bytes(Path::new("downloads").join("archive")))
    .bind("archive.bin")
    .bind(native_path_bytes(
        Path::new("downloads").join("archive").join("archive.bin"),
    ))
    .bind("Remote archive")
    .bind("application/octet-stream")
    .bind("connection ended before completion")
    .execute(pool)
    .await
    .expect("download fixture must be inserted");

    sqlx::query(
        r#"
        INSERT INTO download_headers (download_id, position, name, value, sensitivity)
        VALUES ($1, 0, 'Accept', 'application/octet-stream', 'public')
        "#,
    )
    .bind(DOWNLOAD_ID)
    .execute(pool)
    .await
    .expect("header fixture must be inserted");
}

fn test_id() -> DownloadId {
    DownloadId::new(DOWNLOAD_ID).expect("test ID must be valid")
}

fn valid_tag() -> StrongEntityTag {
    StrongEntityTag::parse(b"\"archive-v1\"").expect("test entity tag must be valid")
}

#[cfg(unix)]
fn native_path_bytes(path: PathBuf) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn native_path_bytes(path: PathBuf) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn native_path_bytes(path: PathBuf) -> Vec<u8> {
    path.to_str()
        .expect("test path must be representable as UTF-8")
        .as_bytes()
        .to_vec()
}

#[tokio::test]
async fn migrations_from_0002_preserve_jobs_and_execution_metadata() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    let pool = apply_first_two_migrations(&database_path).await;

    insert_download_with_metadata(&pool).await;

    let applied_before: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .expect("migration bookkeeping must be readable");
    assert_eq!(applied_before, vec![1, 2]);
    pool.close().await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must upgrade the database");
    let job = repository
        .find_by_id(test_id())
        .await
        .expect("download lookup must succeed")
        .expect("migrated download must exist");

    assert_eq!(
        job.request().url().as_str(),
        "https://example.com/archive.bin"
    );
    assert_eq!(job.request().headers().len(), 1);
    assert_eq!(job.request().headers()[0].name(), "Accept");
    assert_eq!(
        job.request().headers()[0].value(),
        "application/octet-stream"
    );
    assert_eq!(
        job.request().headers()[0].sensitivity(),
        HeaderSensitivity::Public
    );
    assert_eq!(
        job.destination().directory(),
        Path::new("downloads").join("archive")
    );
    assert_eq!(job.destination().preferred_filename(), Some("archive.bin"));
    assert_eq!(
        job.destination().conflict_policy(),
        FileConflictPolicy::Fail
    );
    assert_eq!(job.state(), DownloadState::Failed);
    assert_eq!(job.progress().downloaded_bytes(), 7);
    assert_eq!(job.progress().total_bytes(), Some(12));

    let resource = job.resource().expect("resource metadata must survive");
    assert_eq!(resource.kind(), ResourceKind::File);
    assert_eq!(resource.display_name(), Some("Remote archive"));
    assert_eq!(resource.content_type(), Some("application/octet-stream"));
    assert_eq!(
        job.resolved_destination()
            .expect("resolved destination must survive")
            .final_path(),
        Path::new("downloads").join("archive").join("archive.bin")
    );

    let failure = job.last_failure().expect("failure metadata must survive");
    assert_eq!(failure.kind(), FailureKind::Network);
    assert_eq!(failure.message(), "connection ended before completion");
    assert!(failure.is_retryable());
    repository.close().await;

    let pool = connect(&database_path, false).await;
    let applied_after: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .expect("migration bookkeeping must remain readable");
    assert_eq!(applied_after, vec![1, 2, 3]);
    pool.close().await;
}

#[tokio::test]
async fn reopening_current_database_preserves_existing_records() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    create_current_database(&database_path).await;

    let pool = connect(&database_path, false).await;
    insert_download_with_metadata(&pool).await;
    sqlx::query(
        r#"
        INSERT INTO http_resume_state (download_id, strong_etag, total_bytes, durable_bytes)
        VALUES ($1, $2, 12, 7)
        "#,
    )
    .bind(DOWNLOAD_ID)
    .bind(valid_tag().as_bytes())
    .execute(&pool)
    .await
    .expect("resume fixture must be inserted");
    pool.close().await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("migrated repository must open");
    let expected_job = repository
        .find_by_id(test_id())
        .await
        .expect("download lookup must succeed")
        .expect("download must exist");
    let expected_resume = repository
        .http_resume_store()
        .find(test_id())
        .await
        .expect("resume lookup must succeed")
        .expect("resume state must exist");
    repository.close().await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("already migrated repository must reopen");
    let reopened_job = repository
        .find_by_id(test_id())
        .await
        .expect("download lookup after reopen must succeed")
        .expect("download must remain present");
    let reopened_resume = repository
        .http_resume_store()
        .find(test_id())
        .await
        .expect("resume lookup after reopen must succeed")
        .expect("resume state must remain present");

    assert!(reopened_job == expected_job);
    assert_eq!(reopened_resume, expected_resume);
    repository.close().await;
}

#[tokio::test]
async fn resume_schema_rejects_orphans_and_invalid_checkpoint_counts() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    create_current_database(&database_path).await;
    let pool = connect(&database_path, false).await;

    insert_download_with_metadata(&pool).await;

    let cases = [
        (999, 10, 0, "orphan resume state"),
        (DOWNLOAD_ID, -1, 0, "negative total"),
        (DOWNLOAD_ID, 10, -1, "negative durable checkpoint"),
        (DOWNLOAD_ID, 10, 11, "checkpoint beyond total"),
    ];

    for (download_id, total_bytes, durable_bytes, description) in cases {
        let result = sqlx::query(
            r#"
            INSERT INTO http_resume_state (
                download_id,
                strong_etag,
                total_bytes,
                durable_bytes
            )
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(download_id)
        .bind(valid_tag().as_bytes())
        .bind(total_bytes)
        .bind(durable_bytes)
        .execute(&pool)
        .await;

        assert!(result.is_err(), "SQLite must reject {description}");
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM http_resume_state")
        .fetch_one(&pool)
        .await
        .expect("resume count must be readable");
    assert_eq!(count, 0);
    pool.close().await;
}

#[tokio::test]
async fn malformed_stored_strong_etag_is_invalid_data_and_is_not_disclosed() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    create_current_database(&database_path).await;
    let pool = connect(&database_path, false).await;

    insert_download_with_metadata(&pool).await;
    let malformed = b"private-validator-value";
    sqlx::query(
        r#"
        INSERT INTO http_resume_state (download_id, strong_etag, total_bytes, durable_bytes)
        VALUES ($1, $2, 12, 7)
        "#,
    )
    .bind(DOWNLOAD_ID)
    .bind(malformed.as_slice())
    .execute(&pool)
    .await
    .expect("malformed validator fixture must satisfy the database shape constraint");
    pool.close().await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must open");
    let error = repository
        .http_resume_store()
        .find(test_id())
        .await
        .expect_err("malformed validator must be rejected while loading");

    assert_eq!(error.kind(), RepositoryErrorKind::InvalidData);
    assert_eq!(error.message(), "stored strong HTTP entity tag is invalid");
    let diagnostics = format!("{error} {error:?}");
    assert!(
        !diagnostics
            .as_bytes()
            .windows(malformed.len())
            .any(|part| part == malformed)
    );
    repository.close().await;
}

#[tokio::test]
async fn deleting_download_cascades_to_headers_and_resume_state() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");
    create_current_database(&database_path).await;
    let pool = connect(&database_path, false).await;

    insert_download_with_metadata(&pool).await;
    sqlx::query(
        r#"
        INSERT INTO http_resume_state (download_id, strong_etag, total_bytes, durable_bytes)
        VALUES ($1, $2, 12, 7)
        "#,
    )
    .bind(DOWNLOAD_ID)
    .bind(valid_tag().as_bytes())
    .execute(&pool)
    .await
    .expect("resume fixture must be inserted");

    sqlx::query("DELETE FROM downloads WHERE id = $1")
        .bind(DOWNLOAD_ID)
        .execute(&pool)
        .await
        .expect("download deletion must succeed");

    let headers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM download_headers")
        .fetch_one(&pool)
        .await
        .expect("header count must be readable");
    let resume_states: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM http_resume_state")
        .fetch_one(&pool)
        .await
        .expect("resume count must be readable");
    assert_eq!(headers, 0);
    assert_eq!(resume_states, 0);
    pool.close().await;
}
