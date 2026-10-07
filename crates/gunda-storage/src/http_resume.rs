use gunda_core::application::{RepositoryError, RepositoryErrorKind};
use gunda_core::download::DownloadId;
use gunda_http::{HttpResumeState, HttpResumeStore, StrongEntityTag};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

/// SQLite storage for HTTP representation identity and durable checkpoints.
#[derive(Clone)]
pub struct SqliteHttpResumeStore {
    pool: SqlitePool,
}

impl SqliteHttpResumeStore {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl HttpResumeStore for SqliteHttpResumeStore {
    async fn initialize(
        &self,
        id: DownloadId,
        strong_etag: &StrongEntityTag,
        total_bytes: u64,
    ) -> Result<HttpResumeState, RepositoryError> {
        let total = encode_count(total_bytes)?;

        let result = sqlx::query(
            r#"
        INSERT INTO http_resume_state (
            download_id,
            strong_etag,
            total_bytes,
            durable_bytes
        )
        VALUES ($1, $2, $3, 0)
        ON CONFLICT(download_id) DO NOTHING
        "#,
        )
        .bind(id.value())
        .bind(strong_etag.as_bytes())
        .bind(total)
        .execute(&self.pool)
        .await
        .map_err(|_| internal_error("could not initialize HTTP resume state"))?;

        if result.rows_affected() != 1 {
            return Err(RepositoryError::new(
                RepositoryErrorKind::ConstraintViolation,
                "HTTP resume state already exists",
            ));
        }

        HttpResumeState::new(strong_etag.clone(), total_bytes, 0)
            .map_err(|_| invalid_data("HTTP resume state is inconsistent"))
    }

    async fn find(&self, id: DownloadId) -> Result<Option<HttpResumeState>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT strong_etag, total_bytes, durable_bytes
            FROM http_resume_state
            WHERE download_id = $1
            "#,
        )
        .bind(id.value())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| internal_error("could not load HTTP resume state"))?;

        row.as_ref().map(decode_state).transpose()
    }

    async fn save_checkpoint(
        &self,
        id: DownloadId,
        state: &HttpResumeState,
    ) -> Result<HttpResumeState, RepositoryError> {
        let total = encode_count(state.total_bytes())?;
        let durable = encode_count(state.durable_bytes())?;

        let result = sqlx::query(
            r#"
            UPDATE http_resume_state
            SET durable_bytes = $1
            WHERE download_id = $2
                AND strong_etag = $3
                AND total_bytes = $4
                AND durable_bytes <= $1
            "#,
        )
        .bind(durable)
        .bind(id.value())
        .bind(state.strong_etag().as_bytes())
        .bind(total)
        .execute(&self.pool)
        .await
        .map_err(|_| internal_error("could not save HTTP durable checkpoint"))?;

        if result.rows_affected() == 1 {
            return Ok(state.clone());
        }

        if self.find(id).await?.is_none() {
            return Err(RepositoryError::new(
                RepositoryErrorKind::NotFound,
                "HTTP resume state does not exist",
            ));
        }

        Err(RepositoryError::new(
            RepositoryErrorKind::ConstraintViolation,
            "HTTP checkpoint cannot change representation or move backwards",
        ))
    }
}

fn decode_state(row: &SqliteRow) -> Result<HttpResumeState, RepositoryError> {
    let tag: Vec<u8> = row
        .try_get("strong_etag")
        .map_err(|_| invalid_data("stored HTTP entity tag has invalid column data"))?;

    let total: i64 = row
        .try_get("total_bytes")
        .map_err(|_| invalid_data("stored HTTP total has invalid column data"))?;

    let durable: i64 = row
        .try_get("durable_bytes")
        .map_err(|_| invalid_data("stored HTTP checkpoint has invalid column data"))?;

    let tag = StrongEntityTag::parse(&tag)
        .map_err(|_| invalid_data("stored strong HTTP entity tag is invalid"))?;

    let total = u64::try_from(total).map_err(|_| invalid_data("stored HTTP total is negative"))?;

    let durable =
        u64::try_from(durable).map_err(|_| invalid_data("stored HTTP checkpoint is negative"))?;

    HttpResumeState::new(tag, total, durable)
        .map_err(|_| invalid_data("stored HTTP checkpoint exceeds the total"))
}

fn encode_count(value: u64) -> Result<i64, RepositoryError> {
    i64::try_from(value).map_err(|_| {
        RepositoryError::new(
            RepositoryErrorKind::ConstraintViolation,
            "HTTP byte count exceeds the supported storage range",
        )
    })
}

fn internal_error(message: &'static str) -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::Internal, message)
}

fn invalid_data(message: &'static str) -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::InvalidData, message)
}
