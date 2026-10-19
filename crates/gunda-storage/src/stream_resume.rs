use gunda_core::application::{RepositoryError, RepositoryErrorKind};
use gunda_core::download::DownloadId;
use gunda_stream::{PlanFingerprint, StreamResumeState, StreamResumeStore, TrackCheckpoint};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Clone)]
pub struct SqliteStreamResumeStore {
    pool: SqlitePool,
}

impl SqliteStreamResumeStore {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl StreamResumeStore for SqliteStreamResumeStore {
    async fn initialize(
        &self,
        id: DownloadId,
        fingerprint: PlanFingerprint,
        track_count: usize,
    ) -> Result<StreamResumeState, RepositoryError> {
        if !(1..=2).contains(&track_count) {
            return Err(invalid_data("stream resume state needs one or two tracks"));
        }

        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| internal_error("could not start stream resume initialization"))?;

        let inserted = sqlx::query(
            r#"
            INSERT INTO stream_resume_state (
                download_id,
                plan_fingerprint,
                track_count
            )
            VALUES ($1, $2, $3)
            ON CONFLICT(download_id) DO NOTHING
            "#,
        )
        .bind(id.value())
        .bind(&fingerprint.as_bytes()[..])
        .bind(i64::try_from(track_count).map_err(|_| invalid_data("too many stream tracks"))?)
        .execute(&mut *transaction)
        .await
        .map_err(|_| internal_error("could not initialize stream resume state"))?;

        if inserted.rows_affected() != 1 {
            return Err(RepositoryError::new(
                RepositoryErrorKind::ConstraintViolation,
                "stream resume state already exists",
            ));
        }

        for track in 0..track_count {
            sqlx::query(
                r#"
                INSERT INTO stream_resume_tracks (
                    download_id,
                    track,
                    next_resource,
                    durable_bytes
                )
                VALUES ($1, $2, 0, 0)
                "#,
            )
            .bind(id.value())
            .bind(i64::try_from(track).map_err(|_| invalid_data("invalid stream track"))?)
            .execute(&mut *transaction)
            .await
            .map_err(|_| internal_error("could not initialize stream track checkpoint"))?;
        }

        transaction
            .commit()
            .await
            .map_err(|_| internal_error("could not commit stream resume initialization"))?;

        StreamResumeState::new(
            fingerprint,
            (0..track_count).map(|_| TrackCheckpoint::start()).collect(),
        )
        .map_err(|_| invalid_data("stream resume state is inconsistent"))
    }

    async fn find(&self, id: DownloadId) -> Result<Option<StreamResumeState>, RepositoryError> {
        let state = sqlx::query(
            r#"
            SELECT plan_fingerprint, track_count
            FROM stream_resume_state
            WHERE download_id = $1
            "#,
        )
        .bind(id.value())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| internal_error("could not load stream resume state"))?;

        let Some(state) = state else {
            return Ok(None);
        };

        let fingerprint: Vec<u8> = state
            .try_get("plan_fingerprint")
            .map_err(|_| invalid_data("stored stream fingerprint has invalid column data"))?;
        let fingerprint = PlanFingerprint::parse(&fingerprint)
            .map_err(|_| invalid_data("stored stream fingerprint is invalid"))?;
        let track_count: i64 = state
            .try_get("track_count")
            .map_err(|_| invalid_data("stored stream track count has invalid column data"))?;
        let track_count = usize::try_from(track_count)
            .map_err(|_| invalid_data("stored stream track count is negative"))?;

        let rows = sqlx::query(
            r#"
            SELECT track, next_resource, durable_bytes
            FROM stream_resume_tracks
            WHERE download_id = $1
            ORDER BY track
            "#,
        )
        .bind(id.value())
        .fetch_all(&self.pool)
        .await
        .map_err(|_| internal_error("could not load stream track checkpoints"))?;

        if rows.len() != track_count {
            return Err(invalid_data(
                "stored stream checkpoint has the wrong track count",
            ));
        }

        let tracks = rows
            .iter()
            .enumerate()
            .map(|(index, row)| decode_track(index, row))
            .collect::<Result<Vec<_>, _>>()?;

        StreamResumeState::new(fingerprint, tracks)
            .map(Some)
            .map_err(|_| invalid_data("stored stream checkpoint is inconsistent"))
    }

    async fn save_checkpoint(
        &self,
        id: DownloadId,
        state: &StreamResumeState,
    ) -> Result<StreamResumeState, RepositoryError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| internal_error("could not start stream checkpoint update"))?;

        let stored = sqlx::query(
            r#"
            SELECT track_count
            FROM stream_resume_state
            WHERE download_id = $1 AND plan_fingerprint = $2
            "#,
        )
        .bind(id.value())
        .bind(&state.fingerprint().as_bytes()[..])
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|_| internal_error("could not verify stream resume state"))?;

        let Some(stored) = stored else {
            return Err(RepositoryError::new(
                RepositoryErrorKind::NotFound,
                "stream resume state does not exist or belongs to another plan",
            ));
        };

        let track_count: i64 = stored
            .try_get("track_count")
            .map_err(|_| invalid_data("stored stream track count has invalid column data"))?;
        if usize::try_from(track_count).ok() != Some(state.tracks().len()) {
            return Err(RepositoryError::new(
                RepositoryErrorKind::ConstraintViolation,
                "stream checkpoint cannot change its track count",
            ));
        }

        for (track, checkpoint) in state.tracks().iter().enumerate() {
            let next_resource = i64::try_from(checkpoint.next_resource()).map_err(|_| {
                RepositoryError::new(
                    RepositoryErrorKind::ConstraintViolation,
                    "stream resource index exceeds the supported storage range",
                )
            })?;
            let durable_bytes = encode_count(checkpoint.durable_bytes())?;

            let updated = sqlx::query(
                r#"
                UPDATE stream_resume_tracks
                SET next_resource = $1,
                    durable_bytes = $2
                WHERE download_id = $3
                    AND track = $4
                    AND next_resource <= $1
                    AND durable_bytes <= $2
                "#,
            )
            .bind(next_resource)
            .bind(durable_bytes)
            .bind(id.value())
            .bind(i64::try_from(track).map_err(|_| invalid_data("invalid stream track"))?)
            .execute(&mut *transaction)
            .await
            .map_err(|_| internal_error("could not save stream track checkpoint"))?;

            if updated.rows_affected() != 1 {
                return Err(RepositoryError::new(
                    RepositoryErrorKind::ConstraintViolation,
                    "stream checkpoint cannot move backwards or change tracks",
                ));
            }
        }

        transaction
            .commit()
            .await
            .map_err(|_| internal_error("could not commit stream checkpoint"))?;

        Ok(state.clone())
    }

    async fn clear(&self, id: DownloadId) -> Result<(), RepositoryError> {
        sqlx::query("DELETE FROM stream_resume_state WHERE download_id = $1")
            .bind(id.value())
            .execute(&self.pool)
            .await
            .map_err(|_| internal_error("could not clear stream resume state"))?;
        Ok(())
    }
}

fn decode_track(
    expected_track: usize,
    row: &SqliteRow,
) -> Result<TrackCheckpoint, RepositoryError> {
    let track: i64 = row
        .try_get("track")
        .map_err(|_| invalid_data("stored stream track has invalid column data"))?;
    let track =
        usize::try_from(track).map_err(|_| invalid_data("stored stream track is negative"))?;

    if track != expected_track {
        return Err(invalid_data("stored stream checkpoint has a missing track"));
    }

    let next_resource: i64 = row
        .try_get("next_resource")
        .map_err(|_| invalid_data("stored stream position has invalid column data"))?;
    let next_resource = usize::try_from(next_resource)
        .map_err(|_| invalid_data("stored stream position is negative"))?;
    let durable_bytes: i64 = row
        .try_get("durable_bytes")
        .map_err(|_| invalid_data("stored stream byte count has invalid column data"))?;
    let durable_bytes = u64::try_from(durable_bytes)
        .map_err(|_| invalid_data("stored stream byte count is negative"))?;

    Ok(TrackCheckpoint::new(next_resource, durable_bytes))
}

fn encode_count(value: u64) -> Result<i64, RepositoryError> {
    i64::try_from(value).map_err(|_| {
        RepositoryError::new(
            RepositoryErrorKind::ConstraintViolation,
            "stream byte count exceeds the supported storage range",
        )
    })
}

fn internal_error(message: &'static str) -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::Internal, message)
}

fn invalid_data(message: &'static str) -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::InvalidData, message)
}
