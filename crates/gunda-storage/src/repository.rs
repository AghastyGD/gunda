use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use gunda_core::application::{DownloadRepository, RepositoryError, RepositoryErrorKind};
use gunda_core::download::{
    DownloadDestination, DownloadId, DownloadJob, DownloadOrigin, FileConflictPolicy,
    HeaderSensitivity, NewDownload, RequestContext, RequestHeader,
};
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use time::OffsetDateTime;
use url::Url;

use crate::path_codec;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// SQLx implementation of Gunda's SQLite repository.
pub struct SqliteDownloadRepository {
    pool: SqlitePool,
}

impl SqliteDownloadRepository {
    /// Opens or creates a database and applies pending migrations.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, RepositoryError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));

        Self::connect(options).await
    }

    /// Opens an isolated in-memory database.
    pub async fn open_in_memory() -> Result<Self, RepositoryError> {
        let options = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));

        Self::connect(options).await
    }

    async fn connect(options: SqliteConnectOptions) -> Result<Self, RepositoryError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|_| {
                RepositoryError::new(
                    RepositoryErrorKind::Unavailable,
                    "could not open the download database",
                )
            })?;

        MIGRATOR
            .run(&pool)
            .await
            .map_err(|_| internal_error("could not apply database migrations"))?;

        Ok(Self { pool })
    }

    /// Closes the pool after waiting for checked-out connections.
    pub async fn close(self) {
        self.pool.close().await;
    }
}

impl DownloadRepository for SqliteDownloadRepository {
    async fn create(
        &self,
        download: NewDownload,
        created_at: OffsetDateTime,
    ) -> Result<DownloadJob, RepositoryError> {
        ensure_persistable(&download)?;

        let created_at_unix_ms = encode_timestamp(created_at)?;
        let created_at = decode_timestamp(created_at_unix_ms)?;

        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| internal_error("could not start download creation transaction"))?;

        let id = insert_download(&mut transaction, &download, created_at_unix_ms).await?;

        insert_headers(&mut transaction, id, download.request().headers()).await?;

        transaction
            .commit()
            .await
            .map_err(|_| internal_error("could not commit download creation"))?;

        Ok(DownloadJob::new(id, download, created_at))
    }

    async fn find_by_id(&self, id: DownloadId) -> Result<Option<DownloadJob>, RepositoryError> {
        let row = sqlx::query(
            r#"
                SELECT
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
                    updated_at_unix_ms
                FROM downloads
                WHERE id = $1
                "#,
        )
        .bind(id.value())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| internal_error("could not load download"))?;

        let Some(row) = row else {
            return Ok(None);
        };

        let stored = StoredDownload::from_row(&row)?;
        let stored_id = stored.download_id()?;
        let headers = load_headers(&self.pool, stored_id).await?;
        let job = stored.to_job(stored_id, headers)?;

        Ok(Some(job))
    }

    async fn list(&self) -> Result<Vec<DownloadJob>, RepositoryError> {
        let rows = sqlx::query(
            r#"
        SELECT
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
            updated_at_unix_ms
        FROM downloads
        ORDER BY id
        "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| internal_error("could not list downloads"))?;

        let mut headers_by_download = load_all_headers(&self.pool).await?;
        let mut jobs = Vec::with_capacity(rows.len());

        for row in &rows {
            let stored = StoredDownload::from_row(row)?;
            let id = stored.download_id()?;

            let headers = headers_by_download.remove(&id).unwrap_or_default();
            let job = stored.to_job(id, headers)?;

            jobs.push(job);
        }

        if !headers_by_download.is_empty() {
            return Err(invalid_data(
                "stored request headers reference missing downloads",
            ));
        }

        Ok(jobs)
    }
}

struct StoredDownload {
    id: i64,
    source_url: String,
    origin: String,
    source_page_url: Option<String>,
    source_page_title: Option<String>,
    destination_directory: Vec<u8>,
    preferred_filename: Option<String>,
    conflict_policy: String,
    state: String,
    downloaded_bytes: i64,
    total_bytes: Option<i64>,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
}

impl StoredDownload {
    fn from_row(row: &SqliteRow) -> Result<Self, RepositoryError> {
        let decode = || -> Result<Self, sqlx::Error> {
            Ok(Self {
                id: row.try_get("id")?,
                source_url: row.try_get("source_url")?,
                origin: row.try_get("origin")?,
                source_page_url: row.try_get("source_page_url")?,
                source_page_title: row.try_get("source_page_title")?,
                destination_directory: row.try_get("destination_directory")?,
                preferred_filename: row.try_get("preferred_filename")?,
                conflict_policy: row.try_get("conflict_policy")?,
                state: row.try_get("state")?,
                downloaded_bytes: row.try_get("downloaded_bytes")?,
                total_bytes: row.try_get("total_bytes")?,
                created_at_unix_ms: row.try_get("created_at_unix_ms")?,
                updated_at_unix_ms: row.try_get("updated_at_unix_ms")?,
            })
        };

        decode().map_err(|_| invalid_data("stored download row has invalid column data"))
    }

    fn to_new_download(&self, headers: Vec<RequestHeader>) -> Result<NewDownload, RepositoryError> {
        let url = Url::parse(&self.source_url)
            .map_err(|_| invalid_data("stored source URL is invalid"))?;

        let origin = decode_origin(
            &self.origin,
            self.source_page_url.as_deref(),
            self.source_page_title.clone(),
        )?;

        let directory = path_codec::decode(&self.destination_directory)?;

        let conflict_policy = decode_conflict_policy(&self.conflict_policy)?;

        Ok(NewDownload::new(
            RequestContext::new(url, headers),
            DownloadDestination::new(directory, self.preferred_filename.clone(), conflict_policy),
            origin,
        ))
    }

    fn download_id(&self) -> Result<DownloadId, RepositoryError> {
        DownloadId::new(self.id).map_err(|_| invalid_data("stored donwnload has an invalid ID"))
    }

    fn to_job(
        &self,
        id: DownloadId,
        headers: Vec<RequestHeader>,
    ) -> Result<DownloadJob, RepositoryError> {
        validate_initial_job(self)?;

        let created_at = decode_timestamp(self.created_at_unix_ms)?;
        let download = self.to_new_download(headers)?;

        Ok(DownloadJob::new(id, download, created_at))
    }
}

async fn insert_download(
    transaction: &mut Transaction<'_, Sqlite>,
    download: &NewDownload,
    created_at_unix_ms: i64,
) -> Result<DownloadId, RepositoryError> {
    let (origin, page_url, page_title) = encode_origin(download.origin());

    let destination = download.destination();
    let destination_directory = path_codec::encode(destination.directory())?;

    let result = sqlx::query(
        r#"
        INSERT INTO downloads (
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
            updated_at_unix_ms
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7,
            'queued', 0, NULL, $8, $8
        )
        "#,
    )
    .bind(download.request().url().as_str())
    .bind(origin)
    .bind(page_url)
    .bind(page_title)
    .bind(destination_directory)
    .bind(destination.preferred_filename())
    .bind(encode_conflict_policy(destination.conflict_policy()))
    .bind(created_at_unix_ms)
    .execute(&mut **transaction)
    .await
    .map_err(|_| internal_error("could not insert download record"))?;

    DownloadId::new(result.last_insert_rowid())
        .map_err(|_| invalid_data("database returned an invalid download ID"))
}

async fn insert_headers(
    transaction: &mut Transaction<'_, Sqlite>,
    id: DownloadId,
    headers: &[RequestHeader],
) -> Result<(), RepositoryError> {
    for (position, header) in headers.iter().enumerate() {
        if header.is_sensitive() {
            return Err(RepositoryError::new(
                RepositoryErrorKind::SensitiveDataUnsupported,
                "sensitive request headers cannot be persisted yet",
            ));
        }

        let position = i64::try_from(position).map_err(|_| {
            RepositoryError::new(
                RepositoryErrorKind::ConstraintViolation,
                "request contains too many headers",
            )
        })?;

        sqlx::query(
            r#"
            INSERT INTO download_headers (
                download_id,
                position,
                name,
                value,
                sensitivity
            )
            VALUES ($1, $2, $3, $4, 'public')
            "#,
        )
        .bind(id.value())
        .bind(position)
        .bind(header.name())
        .bind(header.value())
        .execute(&mut **transaction)
        .await
        .map_err(|_| internal_error("could not insert request header"))?;
    }

    Ok(())
}

async fn load_headers(
    pool: &SqlitePool,
    id: DownloadId,
) -> Result<Vec<RequestHeader>, RepositoryError> {
    let rows = sqlx::query(
        r#"
        SELECT name, value, sensitivity
        FROM download_headers
        WHERE download_id = $1
        ORDER BY position
        "#,
    )
    .bind(id.value())
    .fetch_all(pool)
    .await
    .map_err(|_| internal_error("could not query request headers"))?;

    rows.iter().map(decode_header).collect()
}

async fn load_all_headers(
    pool: &SqlitePool,
) -> Result<BTreeMap<DownloadId, Vec<RequestHeader>>, RepositoryError> {
    let rows = sqlx::query(
        r#"
        SELECT
            download_id,
            name,
            value,
            sensitivity
        FROM download_headers
        ORDER BY download_id, position
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|_| internal_error("could not query request headers"))?;

    let mut headers_by_download = BTreeMap::new();

    for row in &rows {
        let raw_download_id: i64 = row
            .try_get("download_id")
            .map_err(|_| invalid_data("stored request header has an invalid download ID"))?;

        let download_id = DownloadId::new(raw_download_id)
            .map_err(|_| invalid_data("stored request header has an invalid download ID"))?;

        let header = decode_header(row)?;

        headers_by_download
            .entry(download_id)
            .or_insert_with(Vec::new)
            .push(header);
    }

    Ok(headers_by_download)
}

fn decode_header(row: &SqliteRow) -> Result<RequestHeader, RepositoryError> {
    let name: String = row
        .try_get("name")
        .map_err(|_| invalid_data("stored request header name is invalid"))?;

    let value: String = row
        .try_get("value")
        .map_err(|_| invalid_data("stored request header value is invalid"))?;

    let sensitivity: String = row
        .try_get("sensitivity")
        .map_err(|_| invalid_data("stored request header sensitivity is invalid"))?;

    if sensitivity != "public" {
        return Err(invalid_data(
            "stored request header has unsupported sensitivity",
        ));
    }

    Ok(RequestHeader::new(name, value, HeaderSensitivity::Public))
}

fn ensure_persistable(download: &NewDownload) -> Result<(), RepositoryError> {
    if download.request().has_sensitive_headers() {
        return Err(RepositoryError::new(
            RepositoryErrorKind::SensitiveDataUnsupported,
            "sensitive request headers cannot be persisted yet",
        ));
    }

    if matches!(download.origin(), DownloadOrigin::Browser { .. }) {
        return Err(RepositoryError::new(
            RepositoryErrorKind::SensitiveDataUnsupported,
            "browser-origin downloads cannot be persisted yet",
        ));
    }

    Ok(())
}

fn encode_origin(origin: &DownloadOrigin) -> (&'static str, Option<&str>, Option<&str>) {
    match origin {
        DownloadOrigin::Desktop => ("desktop", None, None),
        DownloadOrigin::Cli => ("cli", None, None),
        DownloadOrigin::Browser {
            page_url,
            page_title,
        } => ("browser", Some(page_url.as_str()), page_title.as_deref()),
    }
}

fn decode_origin(
    origin: &str,
    page_url: Option<&str>,
    page_title: Option<String>,
) -> Result<DownloadOrigin, RepositoryError> {
    match origin {
        "desktop" if page_url.is_none() && page_title.is_none() => Ok(DownloadOrigin::Desktop),
        "cli" if page_url.is_none() && page_title.is_none() => Ok(DownloadOrigin::Cli),
        "browser" => {
            let page_url =
                page_url.ok_or_else(|| invalid_data("stored browser origin has no page URL"))?;

            let page_url = Url::parse(page_url)
                .map_err(|_| invalid_data("stored browser page URL is invalid"))?;

            Ok(DownloadOrigin::Browser {
                page_url,
                page_title,
            })
        }
        _ => Err(invalid_data("stored download origin is invalid")),
    }
}

const fn encode_conflict_policy(policy: FileConflictPolicy) -> &'static str {
    match policy {
        FileConflictPolicy::Rename => "rename",
        FileConflictPolicy::Overwrite => "overwrite",
        FileConflictPolicy::Fail => "fail",
    }
}

fn decode_conflict_policy(policy: &str) -> Result<FileConflictPolicy, RepositoryError> {
    match policy {
        "rename" => Ok(FileConflictPolicy::Rename),
        "overwrite" => Ok(FileConflictPolicy::Overwrite),
        "fail" => Ok(FileConflictPolicy::Fail),
        _ => Err(invalid_data("stored conflict policy is invalid")),
    }
}

fn validate_initial_job(stored: &StoredDownload) -> Result<(), RepositoryError> {
    if stored.id <= 0 {
        return Err(invalid_data("stored download has an invalid ID"));
    }

    if stored.state != "queued"
        || stored.downloaded_bytes != 0
        || stored.total_bytes.is_some()
        || stored.updated_at_unix_ms != stored.created_at_unix_ms
    {
        return Err(invalid_data("stored download is not an initial queued job"));
    }

    Ok(())
}

fn encode_timestamp(timestamp: OffsetDateTime) -> Result<i64, RepositoryError> {
    let milliseconds = timestamp.unix_timestamp_nanos().div_euclid(1_000_000);

    i64::try_from(milliseconds).map_err(|_| {
        RepositoryError::new(
            RepositoryErrorKind::ConstraintViolation,
            "download timestamp is outside the supported range",
        )
    })
}

fn decode_timestamp(milliseconds: i64) -> Result<OffsetDateTime, RepositoryError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(milliseconds) * 1_000_000)
        .map_err(|_| invalid_data("stored download timestamp is invalid"))
}

fn invalid_data(message: &'static str) -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::InvalidData, message)
}

fn internal_error(message: &'static str) -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::Internal, message)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gunda_core::application::{DownloadRepository, RepositoryErrorKind};
    use gunda_core::download::{
        DownloadDestination, DownloadId, DownloadJob, DownloadOrigin, DownloadState,
        FileConflictPolicy, HeaderSensitivity, NewDownload, RequestContext, RequestHeader,
    };
    use tempfile::tempdir;
    use time::OffsetDateTime;
    use url::Url;

    use super::SqliteDownloadRepository;

    fn sample_download(sensitivity: HeaderSensitivity) -> NewDownload {
        NewDownload::new(
            RequestContext::new(
                Url::parse("https://example.com/file.iso").expect("test URL must be valid"),
                vec![RequestHeader::new(
                    match sensitivity {
                        HeaderSensitivity::Public => "User-Agent",
                        HeaderSensitivity::Sensitive => "Authorization",
                    },
                    match sensitivity {
                        HeaderSensitivity::Public => "Gunda test",
                        HeaderSensitivity::Sensitive => "Bearer secret",
                    },
                    sensitivity,
                )],
            ),
            DownloadDestination::new(
                PathBuf::from("downloads").join("images"),
                Some("file.iso".to_owned()),
                FileConflictPolicy::Rename,
            ),
            DownloadOrigin::Desktop,
        )
    }

    fn browser_download() -> NewDownload {
        NewDownload::new(
            RequestContext::new(
                Url::parse("https://example.com/video.m3u8").expect("test URL must be valid"),
                Vec::new(),
            ),
            DownloadDestination::new(PathBuf::from("downloads"), None, FileConflictPolicy::Rename),
            DownloadOrigin::Browser {
                page_url: Url::parse("https://example.com/watch").expect("page URL must be valid"),
                page_title: Some("Example video".to_owned()),
            },
        )
    }

    fn test_id(value: i64) -> DownloadId {
        DownloadId::new(value).expect("test ID must be valid")
    }

    #[tokio::test]
    async fn created_job_is_persisted_before_it_is_returned() {
        let repository = SqliteDownloadRepository::open_in_memory()
            .await
            .expect("repository must open");

        let created = repository
            .create(
                sample_download(HeaderSensitivity::Public),
                OffsetDateTime::UNIX_EPOCH,
            )
            .await
            .expect("download must be created");

        let loaded = repository
            .find_by_id(created.id())
            .await
            .expect("download query must succeed")
            .expect("download must exist");

        assert!(created == loaded);
        assert_eq!(loaded.state(), DownloadState::Queued);
        assert_eq!(loaded.id(), test_id(1));
    }

    #[tokio::test]
    async fn job_survives_repository_reopen() {
        let directory = tempdir().expect("temporary directory must exist");
        let database_path = directory.path().join("gunda.sqlite3");

        let repository = SqliteDownloadRepository::open(&database_path)
            .await
            .expect("repository must open");

        let created = repository
            .create(
                sample_download(HeaderSensitivity::Public),
                OffsetDateTime::UNIX_EPOCH,
            )
            .await
            .expect("download must be created");

        repository.close().await;

        let repository = SqliteDownloadRepository::open(&database_path)
            .await
            .expect("repository must reopen");

        let loaded = repository
            .find_by_id(created.id())
            .await
            .expect("download query must succeed")
            .expect("download must survive reopen");

        assert!(created == loaded);
    }

    #[tokio::test]
    async fn missing_download_returns_none() {
        let repository = SqliteDownloadRepository::open_in_memory()
            .await
            .expect("repository must open");

        let loaded = repository
            .find_by_id(test_id(404))
            .await
            .expect("download query must succeed");

        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn sensitive_headers_are_rejected_before_persistence() {
        let repository = SqliteDownloadRepository::open_in_memory()
            .await
            .expect("repository must open");

        let result = repository
            .create(
                sample_download(HeaderSensitivity::Sensitive),
                OffsetDateTime::UNIX_EPOCH,
            )
            .await;

        let error = match result {
            Ok(_) => panic!("sensitive headers must be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), RepositoryErrorKind::SensitiveDataUnsupported);

        let loaded = repository
            .find_by_id(test_id(1))
            .await
            .expect("download query must succeed");

        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn browser_origin_is_rejected_until_secure_storage_exists() {
        let repository = SqliteDownloadRepository::open_in_memory()
            .await
            .expect("repository must open");

        let result = repository
            .create(browser_download(), OffsetDateTime::UNIX_EPOCH)
            .await;

        let error = match result {
            Ok(_) => panic!("browser origin must be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), RepositoryErrorKind::SensitiveDataUnsupported);
    }

    #[tokio::test]
    async fn list_restores_all_jobs_after_repository_reopen() {
        let directory = tempdir().expect("temporary directory must exist");
        let database_path = directory.path().join("gunda.sqlite3");

        let repository = SqliteDownloadRepository::open(&database_path)
            .await
            .expect("repository must open");

        let first = repository
            .create(
                sample_download(HeaderSensitivity::Public),
                OffsetDateTime::UNIX_EPOCH,
            )
            .await
            .expect("first download must be created");

        let second = repository
            .create(
                sample_download(HeaderSensitivity::Public),
                OffsetDateTime::UNIX_EPOCH,
            )
            .await
            .expect("second download must be created");

        repository.close().await;

        let repository = SqliteDownloadRepository::open(&database_path)
            .await
            .expect("repository must reopen");

        let loaded = repository.list().await.expect("downloads must be listed");

        let loaded_ids: Vec<DownloadId> = loaded.iter().map(DownloadJob::id).collect();

        assert_eq!(loaded_ids, vec![first.id(), second.id()]);
    }
}
