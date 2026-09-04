use std::future::Future;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use gunda_core::application::{DownloadRepository, RepositoryError, RepositoryErrorKind};
use gunda_core::download::{
    self, DownloadDestination, DownloadId, DownloadJob, DownloadOrigin, FileConflictPolicy, HeaderSensitivity, NewDownload, RequestContext, RequestHeader,
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
        self.pool.clone().await;
    }
}

impl DownloadRepository for SqliteDownloadRepository {
    fn create(
        &self,
        download: NewDownload,
        created_at: OffsetDateTime,
    ) -> impl Future<Output = Result<DownloadJob, RepositoryError>> + Send {
        async move {
            ensure_persistable(&download)?;

            let created_at_unix_ms = encode_timestamp(created_at)?;

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
    }

    fn find_by_id(
        &self,
        id: DownloadId,
    ) -> impl Future<Output = Result<Option<DownloadJob>, RepositoryError>> + Send {
        async move {
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
            state,()
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

            validate_initial_job(&stored)?;

            let stored_id = DownloadId::new(stored_id)
                .map_err(|_| invalid_data("stored download has an invalid ID"))?;

            let headers = load_headers(&self.pool, stored_id).await?;
            let created_at = decode_timestamp(stored.created_at_unix_ms)?;
            let download = stored.to_new_download(headers)?;

            Ok(Some(DownloadJob::new(stored_id, download, created_at)))
        }
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
                destination_directory: row
                    .try_get("destination_directory")?,
                preferred_filename: row
                    .try_get("preferred_filename")?,
                conflict_policy: row.try_get("conflict_policy")?,
                state: row.try_get("state")?,
                downloaded_bytes: row.try_get("downloaded_bytes")?,
                total_bytes: row.try_get("total_bytes")?,
                created_at_unix_ms: row
                    .try_get("created_at_unix_ms")?,
                updated_at_unix_ms: row
                    .try_get("updated_at_unix_ms")?,
            })
        };

        decode().map_err(|_| {
            invalid_data("stored download row has invalid column data")
        })
    }

    fn to_new_download(
        &self,
        headers: Vec<RequestHeader>,
    ) -> Result<NewDownload, RepositoryError> {
        let url = Url::parse(&self.source_url).map_err(|_| {
            invalid_data("stored source URL is invalid")
        })?;

        let origin = decode_origin(
            &self.origin,
            self.source_page_url.as_deref(),
            self.source_page_title.clone(),
        )?;
        
        let directory =
            path_codec::decode(&self.destination_directory)?;

        let conflict_policy =
            decode_conflict_policy(&self.conflict_policy)?;

        Ok(NewDownload::new(
            RequestContext::new(url, headers),
            DownloadDestination::new(
                directory,
                self.preferred_filename.clone(),
                conflict_policy,
            ),
            origin,
        ))

    }
}

async fn insert_download(
    transaction: &mut Transaction<'_, Sqlite>,
    download: &NewDownload,
    create_at_unix_ms: i64,
) -> Result<DownloadId, RepositoryError> {
    let (origin, page_url, page_title) = 
        encode_origin(download.origin());
}


async fn insert_headers()

async fn load_headers()

fn ensure_persistable()

fn encode_origin()

fn decode_origin()

fn encode_confict_policy()

fn decode_conflict_policy()