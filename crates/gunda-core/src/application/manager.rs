use std::collections::BTreeMap;

use time::OffsetDateTime;

use super::{DownloadEvent, DownloadRepository, RepositoryError, RepositoryErrorKind};
use crate::download::{DownloadId, DownloadJob, NewDownload};

/// Coordinates durable download jobs for application clients.
///
/// Jobs become visible through the manager only after their repository
/// operations have completed successfully.
pub struct DownloadManager<R> {
    repository: R,
    jobs: BTreeMap<DownloadId, DownloadJob>,
}

impl<R> DownloadManager<R>
where
    R: DownloadRepository,
{
    /// Loads durable application state before accepting client operations.
    pub async fn start(repository: R) -> Result<Self, RepositoryError> {
        let persisted_jobs = repository.list().await?;
        let mut jobs = BTreeMap::new();

        for job in persisted_jobs {
            let id = job.id();

            if jobs.insert(id, job).is_some() {
                return Err(invalid_startup_state(
                    "repository returned duplicate download IDs",
                ));
            }
        }

        Ok(Self { repository, jobs })
    }

    /// Persists a new job before exposing it through application state.
    pub async fn create(
        &mut self,
        download: NewDownload,
    ) -> Result<DownloadEvent, RepositoryError> {
        let created_at = OffsetDateTime::now_utc();
        let job = self.repository.create(download, created_at).await?;
        let id = job.id();

        if self.jobs.contains_key(&id) {
            return Err(invalid_startup_state(
                "repository returned an existing download ID",
            ));
        }

        self.jobs.insert(id, job);

        Ok(DownloadEvent::Created { id })
    }

    /// Returns an immutable job snapshot owned by the manager.
    #[must_use]
    pub fn job(&self, id: DownloadId) -> Option<&DownloadJob> {
        self.jobs.get(&id)
    }

    /// Iterates over jobs in ascending identifier order.
    pub fn jobs(&self) -> impl Iterator<Item = &DownloadJob> {
        self.jobs.values()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Returns the repository when the composition root shuts the manager down.
    #[must_use]
    pub fn into_repository(self) -> R {
        self.repository
    }
}

fn invalid_startup_state(message: &'static str) -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, Ordering};

    use time::OffsetDateTime;
    use url::Url;

    use super::DownloadManager;
    use crate::application::{
        DownloadEvent, DownloadRepository, RepositoryError, RepositoryErrorKind,
    };
    use crate::download::{
        DownloadDestination, DownloadId, DownloadJob, DownloadOrigin, FileConflictPolicy,
        NewDownload, RequestContext,
    };

    struct FakeRepository {
        jobs: Mutex<Vec<DownloadJob>>,
        next_id: AtomicI64,
        fail_list: bool,
        fail_create: bool,
    }

    impl FakeRepository {
        fn with_jobs(jobs: Vec<DownloadJob>) -> Self {
            let next_id = jobs.iter().map(|job| job.id().value()).max().unwrap_or(0) + 1;

            Self {
                jobs: Mutex::new(jobs),
                next_id: AtomicI64::new(next_id),
                fail_list: false,
                fail_create: false,
            }
        }

        fn failing_list() -> Self {
            Self {
                jobs: Mutex::new(Vec::new()),
                next_id: AtomicI64::new(1),
                fail_list: true,
                fail_create: false,
            }
        }

        fn failing_create() -> Self {
            Self {
                jobs: Mutex::new(Vec::new()),
                next_id: AtomicI64::new(1),
                fail_list: false,
                fail_create: true,
            }
        }
    }

    impl DownloadRepository for FakeRepository {
        async fn create(
            &self,
            download: NewDownload,
            created_at: OffsetDateTime,
        ) -> Result<DownloadJob, RepositoryError> {
            if self.fail_create {
                return Err(RepositoryError::new(
                    RepositoryErrorKind::Unavailable,
                    "repository is unavailable",
                ));
            }

            let raw_id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let id = DownloadId::new(raw_id).expect("fake IDs must be valid");
            let job = DownloadJob::new(id, download, created_at);

            self.jobs
                .lock()
                .expect("fake repository lock must not be poisoned")
                .push(job.clone());

            Ok(job)
        }

        async fn find_by_id(&self, id: DownloadId) -> Result<Option<DownloadJob>, RepositoryError> {
            Ok(self
                .jobs
                .lock()
                .expect("fake repository lock must not be poisoned")
                .iter()
                .find(|job| job.id() == id)
                .cloned())
        }

        async fn list(&self) -> Result<Vec<DownloadJob>, RepositoryError> {
            if self.fail_list {
                return Err(RepositoryError::new(
                    RepositoryErrorKind::Unavailable,
                    "repository is unavailable",
                ));
            }

            Ok(self
                .jobs
                .lock()
                .expect("fake repository lock must not be poisoned")
                .clone())
        }
    }

    fn new_download(filename: &str) -> NewDownload {
        NewDownload::new(
            RequestContext::new(
                Url::parse("https://example.com/file").expect("test URL must be valid"),
                Vec::new(),
            ),
            DownloadDestination::new(
                PathBuf::from("downloads"),
                Some(filename.to_owned()),
                FileConflictPolicy::Rename,
            ),
            DownloadOrigin::Desktop,
        )
    }

    fn job(id: i64) -> DownloadJob {
        DownloadJob::new(
            DownloadId::new(id).expect("test ID must be valid"),
            new_download(&format!("file-{id}.bin")),
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    #[tokio::test]
    async fn startup_loads_jobs_in_identifier_order() {
        let repository = FakeRepository::with_jobs(vec![job(3), job(1), job(2)]);

        let manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        let ids: Vec<i64> = manager.jobs().map(|job| job.id().value()).collect();

        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn startup_failure_does_not_produce_a_manager() {
        let result = DownloadManager::start(FakeRepository::failing_list()).await;

        let error = match result {
            Ok(_) => panic!("manager startup must fail"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), RepositoryErrorKind::Unavailable);
    }

    #[tokio::test]
    async fn duplicate_startup_ids_are_rejected() {
        let repository = FakeRepository::with_jobs(vec![job(1), job(1)]);

        let result = DownloadManager::start(repository).await;

        let error = match result {
            Ok(_) => panic!("duplicate IDs must prevent startup"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), RepositoryErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn created_job_becomes_visible_after_persistence() {
        let repository = FakeRepository::with_jobs(Vec::new());
        let mut manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        let event = manager
            .create(new_download("created.bin"))
            .await
            .expect("creation must succeed");

        let DownloadEvent::Created { id } = event else {
            panic!("manager must produce a Created event");
        };

        assert_eq!(manager.len(), 1);
        assert!(manager.job(id).is_some());
    }

    #[tokio::test]
    async fn failed_creation_does_not_change_manager_state() {
        let repository = FakeRepository::failing_create();
        let mut manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        let result = manager.create(new_download("failed.bin")).await;

        assert!(result.is_err());
        assert!(manager.is_empty());
    }
}
