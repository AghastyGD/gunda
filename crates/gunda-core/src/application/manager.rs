use std::collections::BTreeMap;

use time::OffsetDateTime;

use super::{
    DownloadEvent, DownloadManagerError, DownloadRepository, RepositoryError, RepositoryErrorKind,
};
use crate::{
    application::DownloadCommandKind,
    download::{DownloadId, DownloadJob, DownloadState, NewDownload},
};

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
    #[tracing::instrument(name = "manager.start", skip_all)]
    pub async fn start(repository: R) -> Result<Self, RepositoryError> {
        let result = async {
            let persisted_jobs = repository.list().await?;
            let mut jobs = BTreeMap::new();

            for job in persisted_jobs {
                let id = job.id();

                if jobs.insert(id, job).is_some() {
                    return Err(invalid_repository_data(
                        "repository returned duplicate download IDs",
                    ));
                }
            }

            tracing::info!(jobs_loaded = jobs.len(), "download manager started");

            Ok(Self { repository, jobs })
        }
        .await;

        result.inspect_err(|error| report_manager_error("manager.start", error))
    }

    /// Persists a new job before exposing it through application state.
    #[tracing::instrument(name = "manager.create", skip_all, fields(download_id = tracing::field::Empty))]
    pub async fn create(
        &mut self,
        download: NewDownload,
    ) -> Result<DownloadEvent, RepositoryError> {
        let result = async {
            let created_at = OffsetDateTime::now_utc();
            let job = self.repository.create(download, created_at).await?;
            let id = job.id();

            tracing::Span::current().record("download_id", id.value());

            if self.jobs.contains_key(&id) {
                return Err(invalid_repository_data(
                    "repository returned an existing download ID",
                ));
            }

            let state = job.state();
            self.jobs.insert(id, job);

            tracing::info!(state = ?state, "download registered");

            Ok(DownloadEvent::Created { id })
        }
        .await;

        result.inspect_err(|error| report_manager_error("manager.create", error))
    }

    /// Pauses a queued job before execution begins.
    ///
    /// Pausing active execution requires worker coordination and is not
    /// supported by this operation yet.
    #[tracing::instrument(name = "manager.pause", skip_all, fields(download_id = id.value()))]
    pub async fn pause(&mut self, id: DownloadId) -> Result<DownloadEvent, DownloadManagerError> {
        self.change_idle_state(id, DownloadCommandKind::Pause)
            .await
            .inspect_err(|error| {
                report_management_error("manager.pause", error);
            })
    }

    /// Returns a paused job to the queue.
    ///
    /// This changes scheduling eligibility; it does not start a transfer.
    #[tracing::instrument(name = "manager.resume", skip_all, fields(download_id = id.value()))]
    pub async fn resume(&mut self, id: DownloadId) -> Result<DownloadEvent, DownloadManagerError> {
        self.change_idle_state(id, DownloadCommandKind::Resume)
            .await
            .inspect_err(|error| {
                report_management_error("manager.resume", error);
            })
    }

    /// Cancels a queued or paused job deleting its files or history.
    ///
    /// Cancelling active execution requires worker coordination and is not
    /// supported by this operation yet.
    #[tracing::instrument(name = "manager.cancel", skip_all, fields(download_id = id.value()))]
    pub async fn cancel(&mut self, id: DownloadId) -> Result<DownloadEvent, DownloadManagerError> {
        self.change_idle_state(id, DownloadCommandKind::Cancel)
            .await
            .inspect_err(|error| {
                report_management_error("manager.cancel", error);
            })
    }

    async fn change_idle_state(
        &mut self,
        id: DownloadId,
        command: DownloadCommandKind,
    ) -> Result<DownloadEvent, DownloadManagerError> {
        let current = self
            .jobs
            .get(&id)
            .ok_or(DownloadManagerError::NotFound { id })?;

        let previous = current.state();

        let next = match (command, previous) {
            (DownloadCommandKind::Pause, DownloadState::Queued) => DownloadState::Paused,
            (DownloadCommandKind::Resume, DownloadState::Paused) => DownloadState::Queued,
            (DownloadCommandKind::Cancel, DownloadState::Queued | DownloadState::Paused) => {
                DownloadState::Cancelled
            }
            _ => {
                return Err(DownloadManagerError::InvalidOperation {
                    id,
                    command,
                    state: previous,
                });
            }
        };

        let mut candidate = current.clone();

        candidate
            .transition_to(next, OffsetDateTime::now_utc())
            .map_err(|_| DownloadManagerError::InvalidOperation {
                id,
                command,
                state: previous,
            })?;

        let persisted = self.repository.save(&candidate).await?;

        // The adpter may normalize the update timestam, but must preserve
        // the rest of the submitted aggregate.
        let mut expected = candidate.snapshot();
        expected.updated_at = persisted.updated_at();

        if persisted.snapshot() != expected {
            return Err(invalid_repository_data(
                "repository returned an inconsistent updated download",
            )
            .into());
        }

        self.jobs.insert(id, persisted);

        tracing::info!(
            previous = ?previous,
            current = ?next,
            "download state change committed"
        );

        Ok(DownloadEvent::StateChanged {
            id,
            previous,
            current: next,
        })
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

fn invalid_repository_data(message: &'static str) -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::InvalidData, message)
}

fn report_manager_error(operation: &'static str, error: &RepositoryError) {
    tracing::warn!(
        operation,
        error_kind = ?error.kind(),
        "manager operation failed"
    )
}

fn report_management_error(operation: &'static str, error: &DownloadManagerError) {
    match error {
        DownloadManagerError::NotFound { .. } => {
            tracing::warn!(
                operation,
                error_kind = "NotFound",
                "manager operation failed"
            );
        }
        DownloadManagerError::InvalidOperation { command, state, .. } => {
            tracing::warn!(
                operation,
                error_kind = "InvalidOperation",
                command = ?command,
                state = ?state,
                "manager operation failed"
            );
        }
        DownloadManagerError::Repository(error) => {
            report_manager_error(operation, error);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use time::OffsetDateTime;
    use url::Url;

    use super::DownloadManager;
    use crate::application::{
        DownloadEvent, DownloadManagerError, DownloadRepository, RepositoryError,
        RepositoryErrorKind,
    };
    use crate::download::{
        DownloadDestination, DownloadId, DownloadJob, DownloadOrigin, DownloadState,
        FileConflictPolicy, NewDownload, RequestContext,
    };

    struct FakeRepository {
        jobs: Mutex<Vec<DownloadJob>>,
        next_id: AtomicI64,
        fail_list: bool,
        fail_create: bool,
        fail_save: bool,
        save_calls: AtomicUsize,
    }

    impl FakeRepository {
        fn with_jobs(jobs: Vec<DownloadJob>) -> Self {
            let next_id = jobs.iter().map(|job| job.id().value()).max().unwrap_or(0) + 1;

            Self {
                jobs: Mutex::new(jobs),
                next_id: AtomicI64::new(next_id),
                fail_list: false,
                fail_create: false,
                fail_save: false,
                save_calls: AtomicUsize::new(0),
            }
        }

        fn failing_list() -> Self {
            Self {
                jobs: Mutex::new(Vec::new()),
                next_id: AtomicI64::new(1),
                fail_list: true,
                fail_create: false,
                fail_save: false,
                save_calls: AtomicUsize::new(0),
            }
        }

        fn failing_create() -> Self {
            Self {
                jobs: Mutex::new(Vec::new()),
                next_id: AtomicI64::new(1),
                fail_list: false,
                fail_create: true,
                fail_save: false,
                save_calls: AtomicUsize::new(0),
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

        async fn save(&self, job: &DownloadJob) -> Result<DownloadJob, RepositoryError> {
            self.save_calls.fetch_add(1, Ordering::Relaxed);

            if self.fail_save {
                return Err(RepositoryError::new(
                    RepositoryErrorKind::Unavailable,
                    "repository is unavailable",
                ));
            }

            let mut jobs = self
                .jobs
                .lock()
                .expect("fake repository lock must not be poisoned");

            let current = jobs
                .iter_mut()
                .find(|current| current.id() == job.id())
                .ok_or_else(|| {
                    RepositoryError::new(RepositoryErrorKind::NotFound, "download does not exist")
                })?;

            if current.request() != job.request()
                || current.origin() != job.origin()
                || current.destination() != job.destination()
                || current.created_at() != job.created_at()
            {
                return Err(RepositoryError::new(
                    RepositoryErrorKind::ConstraintViolation,
                    "download creation metadata cannot be changed",
                ));
            }

            *current = job.clone();

            Ok(current.clone())
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

    #[tokio::test]
    async fn duplicate_created_id_does_not_replace_an_existing_job() {
        let original = job(1);
        let repository = FakeRepository::with_jobs(vec![original.clone()]);

        repository.next_id.store(1, Ordering::Relaxed);

        let mut manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        let error = match manager.create(new_download("replacement.bin")).await {
            Ok(_) => panic!("duplicate ID must not produce a success event"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), RepositoryErrorKind::InvalidData);
        assert_eq!(manager.len(), 1);
        assert!(manager.job(original.id()) == Some(&original));
    }

    #[tokio::test]
    async fn idle_operations_persist_before_returning_events() {
        let original = job(1);
        let id = original.id();

        let repository = FakeRepository::with_jobs(vec![original]);
        let mut manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        let event = manager.pause(id).await.expect("pause must succeed");

        assert!(
            event
                == DownloadEvent::StateChanged {
                    id,
                    previous: DownloadState::Queued,
                    current: DownloadState::Paused
                }
        );

        let persisted = manager
            .repository
            .find_by_id(id)
            .await
            .expect("lookup must succeed")
            .expect("job must exist");

        assert_eq!(persisted.state(), DownloadState::Paused);
        assert!(manager.job(id) == Some(&persisted));

        let event = manager.resume(id).await.expect("resume must succeed");

        assert!(
            event
                == DownloadEvent::StateChanged {
                    id,
                    previous: DownloadState::Paused,
                    current: DownloadState::Queued,
                }
        );

        let persisted = manager
            .repository
            .find_by_id(id)
            .await
            .expect("lookup must succeed")
            .expect("job must exist");

        assert_eq!(persisted.state(), DownloadState::Queued);
        assert!(manager.job(id) == Some(&persisted));

        let event = manager.cancel(id).await.expect("cancel must succeed");

        assert!(
            event
                == DownloadEvent::StateChanged {
                    id,
                    previous: DownloadState::Queued,
                    current: DownloadState::Cancelled,
                }
        );

        let persisted = manager
            .repository
            .find_by_id(id)
            .await
            .expect("lookup must succeed")
            .expect("job must exist");

        assert_eq!(persisted.state(), DownloadState::Cancelled);
        assert!(manager.job(id) == Some(&persisted));

        assert_eq!(manager.repository.save_calls.load(Ordering::Relaxed), 3,);
    }

    #[tokio::test]
    async fn failed_pause_preserves_memory_and_persisted_state() {
        let original = job(1);
        let id = original.id();

        let mut repository = FakeRepository::with_jobs(vec![original.clone()]);
        repository.fail_save = true;

        let mut manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        let result = manager.pause(id).await;

        assert!(matches!(
            result,
            Err(DownloadManagerError::Repository(ref error))
                if error.kind() == RepositoryErrorKind::Unavailable
        ));

        assert!(manager.job(id) == Some(&original));

        let persisted = manager
            .repository
            .find_by_id(id)
            .await
            .expect("lookup must succeed")
            .expect("job must exist");

        assert!(persisted == original);
        assert_eq!(manager.repository.save_calls.load(Ordering::Relaxed), 1,);
    }

    #[tokio::test]
    async fn invalid_operations_do_not_call_storage() {
        let original = job(1);
        let id = original.id();

        let repository = FakeRepository::with_jobs(vec![original.clone()]);
        let mut manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        let result = manager.resume(id).await;

        assert!(matches!(
            result,
            Err(DownloadManagerError::InvalidOperation {
                state: DownloadState::Queued,
                ..
            })
        ));

        assert!(manager.job(id) == Some(&original));
        assert_eq!(manager.repository.save_calls.load(Ordering::Relaxed), 0,);
    }

    #[tokio::test]
    async fn missing_job_does_not_call_storage() {
        let repository = FakeRepository::with_jobs(Vec::new());
        let mut manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        let id = DownloadId::new(404).expect("ID must be valid");

        assert!(matches!(
            manager.pause(id).await,
            Err(DownloadManagerError::NotFound { id: missing })
                if missing == id
        ));

        assert_eq!(manager.repository.save_calls.load(Ordering::Relaxed), 0,);
    }

    #[tokio::test]
    async fn active_jobs_require_worker_coordination() {
        let mut active = job(1);
        let id = active.id();

        active
            .transition_to(DownloadState::Inspecting, OffsetDateTime::UNIX_EPOCH)
            .expect("inspection transition must succeed");

        active
            .transition_to(DownloadState::Downloading, OffsetDateTime::UNIX_EPOCH)
            .expect("download transition must succeed");

        let repository = FakeRepository::with_jobs(vec![active.clone()]);
        let mut manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        assert!(matches!(
            manager.pause(id).await,
            Err(DownloadManagerError::InvalidOperation { .. })
        ));

        assert!(matches!(
            manager.cancel(id).await,
            Err(DownloadManagerError::InvalidOperation { .. })
        ));

        assert!(manager.job(id) == Some(&active));
        assert_eq!(manager.repository.save_calls.load(Ordering::Relaxed), 0,);
    }

    #[tokio::test]
    async fn cancelled_job_cannot_return_to_the_queue() {
        let original = job(1);
        let id = original.id();

        let repository = FakeRepository::with_jobs(vec![original]);
        let mut manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        manager.pause(id).await.expect("pause must succeed");
        manager.cancel(id).await.expect("cancel must succeed");

        assert!(matches!(
            manager.resume(id).await,
            Err(DownloadManagerError::InvalidOperation {
                state: DownloadState::Cancelled,
                ..
            })
        ));

        assert_eq!(
            manager.job(id).expect("job must exist").state(),
            DownloadState::Cancelled,
        );

        assert_eq!(manager.repository.save_calls.load(Ordering::Relaxed), 2,);
    }
}
