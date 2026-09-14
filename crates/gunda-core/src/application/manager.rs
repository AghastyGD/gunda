use std::collections::BTreeMap;

use time::OffsetDateTime;

use super::{
    DownloadEvent, DownloadExecutor, DownloadManagerError, DownloadRepository, ExecutionInput,
    ExecutionReport, PreparedTransfer, RepositoryError, RepositoryErrorKind, StagedTransfer,
};

use crate::{
    application::DownloadCommandKind,
    download::{
        DownloadFailure, DownloadId, DownloadJob, DownloadProgress, DownloadState, NewDownload,
    },
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

    /// Cancels a queued or paused job without deleting its files or history.
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

    /// Executes a qued download and persists each phase.
    #[tracing::instrument(name = "manager.execute", skip_all, fields(download_id = id.value()))]
    pub async fn execute<E>(
        &mut self,
        id: DownloadId,
        executor: &E,
    ) -> Result<ExecutionReport, DownloadManagerError>
    where
        E: DownloadExecutor,
    {
        self.execute_inner(id, executor).await.inspect_err(|error| {
            report_management_error("manager.execute", error);
        })
    }

    async fn execute_inner<E>(
        &mut self,
        id: DownloadId,
        executor: &E,
    ) -> Result<ExecutionReport, DownloadManagerError>
    where
        E: DownloadExecutor,
    {
        let mut job = self
            .jobs
            .get(&id)
            .cloned()
            .ok_or(DownloadManagerError::NotFound { id })?;

        if job.state() != DownloadState::Queued {
            return Err(DownloadManagerError::InvalidExecutionState {
                id,
                state: job.state(),
            });
        }

        execution_transition(&mut job, DownloadState::Inspecting)?;
        job = self.commit_job(job).await?;

        let input = ExecutionInput {
            id,
            request: job.request().clone(),
            destination: job.destination().clone(),
        };

        let prepared = match executor.prepare(input).await {
            Ok(prepared) => prepared,
            Err(failure) => {
                return self.record_execution_failure(id, failure).await;
            }
        };

        let total_bytes = prepared.total_bytes();

        let initial_progress = DownloadProgress::new(0, total_bytes)
            .map_err(|_| invalid_repository_data("execution progress is invalid"))?;

        job.set_resource(prepared.resource(), OffsetDateTime::now_utc());
        job.update_progress(initial_progress, OffsetDateTime::now_utc());

        execution_transition(&mut job, DownloadState::Downloading)?;
        job = self.commit_job(job).await?;

        let staged = match prepared.transfer().await {
            Ok(staged) => staged,
            Err(failure) => {
                return self.record_execution_failure(id, failure).await;
            }
        };

        let written_bytes = staged.written_bytes();

        let progress = DownloadProgress::new(written_bytes, total_bytes)
            .map_err(|_| invalid_repository_data("executor returned invalid progress"))?;

        if let Some(total) = total_bytes
            && written_bytes != total
        {
            return Err(
                invalid_repository_data("executor returned an incomplete staged transfer").into(),
            );
        }

        job.update_progress(progress, OffsetDateTime::now_utc());

        execution_transition(&mut job, DownloadState::Finalizing)?;
        job = self.commit_job(job).await?;

        let output = match staged.finalize().await {
            Ok(output) => output,
            Err(failure) => {
                return self.record_execution_failure(id, failure).await;
            }
        };

        // Publication cannot be undone by a later database failure.
        let completion_error = |error| DownloadManagerError::CompletionNotPersisted {
            id,
            destination: output.destination.clone(),
            written_bytes: output.written_bytes,
            cleanup_pending: output.cleanup_pending,
            error,
        };

        if output.written_bytes != written_bytes {
            return Err(completion_error(invalid_repository_data(
                "executor returned inconsistent published output",
            )));
        }

        job.resolve_destination(output.destination.clone(), OffsetDateTime::now_utc());

        execution_transition(&mut job, DownloadState::Completed).map_err(completion_error)?;

        self.commit_job(job).await.map_err(completion_error)?;

        tracing::info!(
            cleanup_pending = output.cleanup_pending,
            "download execution completed"
        );

        Ok(ExecutionReport {
            event: DownloadEvent::Completed {
                id,
                destination: output.destination,
            },
            cleanup_pending: output.cleanup_pending,
        })
    }

    async fn record_execution_failure(
        &mut self,
        id: DownloadId,
        failure: DownloadFailure,
    ) -> Result<ExecutionReport, DownloadManagerError> {
        let mut job = self
            .jobs
            .get(&id)
            .cloned()
            .ok_or(DownloadManagerError::NotFound { id })?;

        job.fail(failure.clone(), OffsetDateTime::now_utc())
            .map_err(|_| {
                invalid_repository_data("execution failure cannot be applied to the current state")
            })?;

        self.commit_job(job).await?;

        tracing::warn!(
            failure_kind = ?failure.kind(),
            "download execution failure committed"
        );

        Ok(ExecutionReport {
            event: DownloadEvent::Failed { id, failure },
            cleanup_pending: false,
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

        self.commit_job(candidate).await?;

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

    async fn commit_job(&mut self, candidate: DownloadJob) -> Result<DownloadJob, RepositoryError> {
        let id = candidate.id();
        let persisted = self.repository.save(&candidate).await?;

        let mut expected = candidate.snapshot();
        expected.updated_at = persisted.updated_at();

        if persisted.snapshot() != expected {
            return Err(invalid_repository_data(
                "repository returned an inconsistent updated download",
            ));
        }

        self.jobs.insert(id, persisted.clone());

        Ok(persisted)
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

fn execution_transition(job: &mut DownloadJob, next: DownloadState) -> Result<(), RepositoryError> {
    job.transition_to(next, OffsetDateTime::now_utc())
        .map_err(|_| invalid_repository_data("execution requested an invalid lifecycle transition"))
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
        DownloadManagerError::InvalidExecutionState { state, .. } => {
            tracing::warn!(
                operation,
                error_kind = "InvalidExecutionState",
                state = ?state,
                "manager operation failed"
            );
        }

        DownloadManagerError::CompletionNotPersisted { error, .. } => {
            tracing::error!(
                operation,
                error_kind = ?error.kind(),
                output_published = true,
                "download completion was not persisted"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use time::OffsetDateTime;
    use url::Url;

    use super::DownloadManager;
    use crate::application::{
        DownloadEvent, DownloadExecutor, DownloadManagerError, DownloadRepository, ExecutionInput,
        ExecutionOutput, PreparedTransfer, RepositoryError, RepositoryErrorKind, StagedTransfer,
    };
    use crate::download::{
        DownloadDestination, DownloadFailure, DownloadId, DownloadJob, DownloadOrigin,
        DownloadState, FailureKind, FileConflictPolicy, NewDownload, RequestContext,
        ResolvedDestination, ResourceDescriptor, ResourceKind,
    };

    struct FakeRepository {
        jobs: Mutex<Vec<DownloadJob>>,
        next_id: AtomicI64,
        fail_list: bool,
        fail_create: bool,
        fail_save: bool,
        save_calls: Arc<AtomicUsize>,
        fail_save_at: Option<DownloadState>,
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
                save_calls: Arc::new(AtomicUsize::new(0)),
                fail_save_at: None,
            }
        }

        fn failing_list() -> Self {
            Self {
                jobs: Mutex::new(Vec::new()),
                next_id: AtomicI64::new(1),
                fail_list: true,
                fail_create: false,
                fail_save: false,
                save_calls: Arc::new(AtomicUsize::new(0)),
                fail_save_at: None,
            }
        }

        fn failing_create() -> Self {
            Self {
                jobs: Mutex::new(Vec::new()),
                next_id: AtomicI64::new(1),
                fail_list: false,
                fail_create: true,
                fail_save: false,
                save_calls: Arc::new(AtomicUsize::new(0)),
                fail_save_at: None,
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

            if self.fail_save || self.fail_save_at == Some(job.state()) {
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

    #[derive(Clone)]
    struct ExecutionProbe {
        saves: Arc<AtomicUsize>,
        calls: Arc<Mutex<Vec<&'static str>>>,
        fail_phase: Option<&'static str>,
    }

    impl ExecutionProbe {
        fn enter(&self, phase: &'static str, expected_saves: usize) -> Result<(), DownloadFailure> {
            assert_eq!(
                self.saves.load(Ordering::Relaxed),
                expected_saves,
                "execution phase started before the expected persistence",
            );

            self.calls
                .lock()
                .expect("probe lock must not be poisoned")
                .push(phase);

            if self.fail_phase == Some(phase) {
                return Err(DownloadFailure::new(
                    FailureKind::Network,
                    "simulated execution failure",
                    true,
                ));
            }

            Ok(())
        }
    }
    impl DownloadExecutor for ExecutionProbe {
        type Prepared = Self;

        async fn prepare(&self, _input: ExecutionInput) -> Result<Self::Prepared, DownloadFailure> {
            self.enter("prepare", 1)?;
            Ok(self.clone())
        }
    }

    impl PreparedTransfer for ExecutionProbe {
        type Staged = Self;

        fn resource(&self) -> ResourceDescriptor {
            ResourceDescriptor::new(ResourceKind::File, None, None)
        }

        fn total_bytes(&self) -> Option<u64> {
            Some(5)
        }

        async fn transfer(self) -> Result<Self::Staged, DownloadFailure> {
            self.enter("transfer", 2)?;
            Ok(self)
        }
    }

    impl StagedTransfer for ExecutionProbe {
        fn written_bytes(&self) -> u64 {
            5
        }

        async fn finalize(self) -> Result<ExecutionOutput, DownloadFailure> {
            self.enter("finalize", 3)?;

            Ok(ExecutionOutput {
                destination: ResolvedDestination::new(
                    PathBuf::from("downloads").join("result.bin"),
                ),
                written_bytes: 5,
                cleanup_pending: false,
            })
        }
    }

    async fn execution_fixture(
        fail_save_at: Option<DownloadState>,
        fail_phase: Option<&'static str>,
    ) -> (DownloadManager<FakeRepository>, ExecutionProbe, DownloadId) {
        let initial = job(1);
        let id = initial.id();

        let mut repository = FakeRepository::with_jobs(vec![initial]);
        repository.fail_save_at = fail_save_at;

        let executor = ExecutionProbe {
            saves: Arc::clone(&repository.save_calls),
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_phase,
        };

        let manager = DownloadManager::start(repository)
            .await
            .expect("manager must start");

        (manager, executor, id)
    }

    #[tokio::test]
    async fn execution_persists_each_phase_before_advancing() {
        let (mut manager, executor, id) = execution_fixture(None, None).await;

        let report = manager
            .execute(id, &executor)
            .await
            .expect("execution must succeed");

        assert!(matches!(
            report.event,
            DownloadEvent::Completed { id: completed, .. } if completed == id
        ));

        let current = manager.job(id).expect("job must exist");

        assert_eq!(current.state(), DownloadState::Completed);
        assert_eq!(current.progress().downloaded_bytes(), 5);
        assert_eq!(current.progress().total_bytes(), Some(5));
        assert!(current.resolved_destination().is_some());

        let persisted = manager
            .repository
            .find_by_id(id)
            .await
            .expect("lookup must succeed")
            .expect("job must exist");

        assert!(current == &persisted);
        assert_eq!(executor.saves.load(Ordering::Relaxed), 4);

        assert_eq!(
            *executor
                .calls
                .lock()
                .expect("probe lock must not be poisoned"),
            vec!["prepare", "transfer", "finalize"],
        );

        assert!(matches!(
            manager.execute(id, &executor).await,
            Err(DownloadManagerError::InvalidExecutionState {
                state: DownloadState::Completed,
                ..
            })
        ));

        assert_eq!(executor.saves.load(Ordering::Relaxed), 4);
    }

    #[tokio::test]
    async fn persistence_failure_stops_execution_at_the_phase_boundary() {
        let cases = [
            (DownloadState::Inspecting, DownloadState::Queued, vec![]),
            (
                DownloadState::Downloading,
                DownloadState::Inspecting,
                vec!["prepare"],
            ),
            (
                DownloadState::Finalizing,
                DownloadState::Downloading,
                vec!["prepare", "transfer"],
            ),
            (
                DownloadState::Completed,
                DownloadState::Finalizing,
                vec!["prepare", "transfer", "finalize"],
            ),
        ];

        for (failed_save, expected_state, expected_calls) in cases {
            let (mut manager, executor, id) = execution_fixture(Some(failed_save), None).await;

            let error = match manager.execute(id, &executor).await {
                Ok(_) => panic!("persistence must fail"),
                Err(error) => error,
            };

            if failed_save == DownloadState::Completed {
                assert!(matches!(
                    error,
                    DownloadManagerError::CompletionNotPersisted {
                        written_bytes: 5,
                        ..
                    }
                ));
            } else {
                assert!(matches!(error, DownloadManagerError::Repository(_)));
            }

            let current = manager.job(id).expect("job must exist");
            assert_eq!(current.state(), expected_state);

            let persisted = manager
                .repository
                .find_by_id(id)
                .await
                .expect("lookup must succeed")
                .expect("job must exist");

            assert!(current == &persisted);

            assert_eq!(
                *executor
                    .calls
                    .lock()
                    .expect("probe lock must not be poisoned"),
                expected_calls,
            );
        }
    }

    #[tokio::test]
    async fn executor_failures_are_persisted_before_returning_failed() {
        for phase in ["prepare", "transfer", "finalize"] {
            let (mut manager, executor, id) = execution_fixture(None, Some(phase)).await;

            let report = manager
                .execute(id, &executor)
                .await
                .expect("failure must be persisted");

            assert!(matches!(
                report.event,
                DownloadEvent::Failed { id: failed, .. } if failed == id
            ));

            let current = manager.job(id).expect("job must exist");

            assert_eq!(current.state(), DownloadState::Failed);
            assert_eq!(
                current.last_failure().expect("failure must exist").kind(),
                FailureKind::Network,
            );

            let persisted = manager
                .repository
                .find_by_id(id)
                .await
                .expect("lookup must succeed")
                .expect("job must exist");

            assert!(current == &persisted);
        }
    }

    #[tokio::test]
    async fn failure_recording_error_does_not_return_a_failed_event() {
        let (mut manager, executor, id) =
            execution_fixture(Some(DownloadState::Failed), Some("transfer")).await;

        assert!(matches!(
            manager.execute(id, &executor).await,
            Err(DownloadManagerError::Repository(_))
        ));

        assert_eq!(
            manager.job(id).expect("job must exist").state(),
            DownloadState::Downloading,
        );
    }

    #[tokio::test]
    async fn paused_and_missing_jobs_do_not_start_execution() {
        let (mut manager, executor, id) = execution_fixture(None, None).await;

        manager.pause(id).await.expect("pause must succeed");

        assert!(matches!(
            manager.execute(id, &executor).await,
            Err(DownloadManagerError::InvalidExecutionState {
                state: DownloadState::Paused,
                ..
            })
        ));

        let missing = DownloadId::new(404).expect("ID must be valid");

        assert!(matches!(
            manager.execute(missing, &executor).await,
            Err(DownloadManagerError::NotFound { .. })
        ));

        assert!(
            executor
                .calls
                .lock()
                .expect("probe lock must not be poisoned")
                .is_empty()
        );
    }
}
