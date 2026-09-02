use time::OffsetDateTime;

use super::{
    DownloadDestination, DownloadFailure, DownloadId, DownloadOrigin, DownloadProgress,
    DownloadState, InvalidStateTransition, RequestContext, ResolvedDestination, ResourceDescriptor,
};

/// Input used to create a persistent download job.
///
/// It intentionally has no `DownloadId` because storage assigns the ID.
#[derive(Clone, PartialEq, Eq)]
pub struct NewDownload {
    request: RequestContext,
    destination: DownloadDestination,
    origin: DownloadOrigin,
}

impl NewDownload {
    #[must_use]
    pub fn new(
        request: RequestContext,
        destination: DownloadDestination,
        origin: DownloadOrigin,
    ) -> Self {
        Self {
            request,
            destination,
            origin,
        }
    }

    #[must_use]
    pub const fn request(&self) -> &RequestContext {
        &self.request
    }

    #[must_use]
    pub const fn destination(&self) -> &DownloadDestination {
        &self.destination
    }

    #[must_use]
    pub const fn origin(&self) -> &DownloadOrigin {
        &self.origin
    }
}

/// Primary persistent aggregate representing a download.
#[derive(Clone, PartialEq, Eq)]
pub struct DownloadJob {
    id: DownloadId,
    request: RequestContext,
    origin: DownloadOrigin,
    destination: DownloadDestination,
    resolved_destination: Option<ResolvedDestination>,
    resource: Option<ResourceDescriptor>,
    state: DownloadState,
    progress: DownloadProgress,
    last_failure: Option<DownloadFailure>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl DownloadJob {
    /// Creates the initial aggregate after storage assigns an ID.
    #[must_use]
    pub fn new(id: DownloadId, new_download: NewDownload, created_at: OffsetDateTime) -> Self {
        Self {
            id,
            request: new_download.request,
            origin: new_download.origin,
            destination: new_download.destination,
            resolved_destination: None,
            resource: None,
            state: DownloadState::Queued,
            progress: DownloadProgress::default(),
            last_failure: None,
            created_at,
            updated_at: created_at,
        }
    }

    #[must_use]
    pub const fn id(&self) -> DownloadId {
        self.id
    }

    #[must_use]
    pub const fn request(&self) -> &RequestContext {
        &self.request
    }

    #[must_use]
    pub const fn origin(&self) -> &DownloadOrigin {
        &self.origin
    }

    #[must_use]
    pub const fn destination(&self) -> &DownloadDestination {
        &self.destination
    }

    #[must_use]
    pub const fn resolved_destination(&self) -> Option<&ResolvedDestination> {
        self.resolved_destination.as_ref()
    }

    #[must_use]
    pub const fn resource(&self) -> Option<&ResourceDescriptor> {
        self.resource.as_ref()
    }

    #[must_use]
    pub const fn state(&self) -> DownloadState {
        self.state
    }

    #[must_use]
    pub const fn progress(&self) -> DownloadProgress {
        self.progress
    }

    #[must_use]
    pub const fn last_failure(&self) -> Option<&DownloadFailure> {
        self.last_failure.as_ref()
    }

    #[must_use]
    pub const fn created_at(&self) -> OffsetDateTime {
        self.created_at
    }

    #[must_use]
    pub const fn updated_at(&self) -> OffsetDateTime {
        self.updated_at
    }

    /// Applies a validated lifecycle transition.
    ///
    /// The application layer remains responsible for persistence and event
    /// ordering around this change.
    pub fn transition_to(
        &mut self,
        next: DownloadState,
        updated_at: OffsetDateTime,
    ) -> Result<(), InvalidStateTransition> {
        self.state.ensure_can_transition_to(next)?;

        self.state = next;
        self.updated_at = updated_at;

        Ok(())
    }

    /// Replaces the durable progress checkpoint.
    pub fn update_progress(&mut self, progress: DownloadProgress, updated_at: OffsetDateTime) {
        self.progress = progress;
        self.updated_at = updated_at
    }

    /// Records generic metadata discovered during inspection.
    pub fn set_resource(&mut self, resource: ResourceDescriptor, updated_at: OffsetDateTime) {
        self.resource = Some(resource);
        self.updated_at = updated_at;
    }

    /// Records the final destination selected after inspection.
    pub fn resolve_destination(
        &mut self,
        destination: ResolvedDestination,
        updated_at: OffsetDateTime,
    ) {
        self.resolved_destination = Some(destination);
        self.updated_at = updated_at;
    }

    /// Transitions the job to `Failed` and records its safe failure details.
    pub fn fail(
        &mut self,
        failure: DownloadFailure,
        updated_at: OffsetDateTime,
    ) -> Result<(), InvalidStateTransition> {
        self.state.ensure_can_transition_to(DownloadState::Failed)?;

        self.state = DownloadState::Failed;
        self.last_failure = Some(failure);
        self.updated_at = updated_at;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use time::{Duration, OffsetDateTime};
    use url::Url;

    use super::{DownloadJob, NewDownload};
    use crate::download::{
        DownloadDestination, DownloadFailure, DownloadId, DownloadOrigin, DownloadProgress,
        DownloadState, FailureKind, FileConflictPolicy, RequestContext, ResolvedDestination,
        ResourceDescriptor, ResourceKind,
    };

    fn sample_new_download() -> NewDownload {
        let request = RequestContext::new(
            Url::parse("https://example.com/image.iso").expect("test URL must be valid"),
            Vec::new(),
        );

        let destination = DownloadDestination::new(
            PathBuf::from("downloads"),
            Some("image.iso".to_owned()),
            FileConflictPolicy::Rename,
        );

        NewDownload::new(request, destination, DownloadOrigin::Desktop)
    }

    fn sample_job() -> DownloadJob {
        DownloadJob::new(
            DownloadId::new(1).expect("test ID must be valid"),
            sample_new_download(),
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn new_job_starts_as_an_unresolved_queued_download() {
        let job = sample_job();

        assert_eq!(job.id().value(), 1);
        assert_eq!(job.state(), DownloadState::Queued);
        assert_eq!(job.progress(), DownloadProgress::default());
        assert!(job.resource().is_none());
        assert!(job.resolved_destination().is_none());
        assert!(job.last_failure().is_none());
        assert_eq!(job.created_at(), OffsetDateTime::UNIX_EPOCH);
        assert_eq!(job.updated_at(), OffsetDateTime::UNIX_EPOCH);
        assert_eq!(
            job.request().url().as_str(),
            "https://example.com/image.iso"
        );
    }

    #[test]
    fn valid_transition_updates_state_and_timestamp() {
        let mut job = sample_job();
        let updated_at = OffsetDateTime::UNIX_EPOCH + Duration::seconds(1);

        job.transition_to(DownloadState::Inspecting, updated_at)
            .expect("Queued -> Inspecting must be valid");

        assert_eq!(job.state(), DownloadState::Inspecting);
        assert_eq!(job.updated_at(), updated_at);
    }

    #[test]
    fn invalid_transition_leaves_job_unchanged() {
        let mut job = sample_job();
        let original_updated_at = job.updated_at();

        let result = job.transition_to(
            DownloadState::Completed,
            OffsetDateTime::UNIX_EPOCH + Duration::seconds(1),
        );

        assert!(result.is_err());
        assert_eq!(job.state(), DownloadState::Queued);
        assert_eq!(job.updated_at(), original_updated_at);
    }

    #[test]
    fn inspection_metadata_can_be_attached_to_job() {
        let mut job = sample_job();
        let updated_at = OffsetDateTime::UNIX_EPOCH + Duration::seconds(1);

        job.set_resource(
            ResourceDescriptor::new(
                ResourceKind::File,
                Some("image.iso".to_owned()),
                Some("application/octet-stream".to_owned()),
            ),
            updated_at,
        );

        job.resolve_destination(
            ResolvedDestination::new(PathBuf::from("downloads").join("image.iso")),
            updated_at,
        );

        let resource = job.resource().expect("resource must be resolved");
        let destination = job
            .resolved_destination()
            .expect("destination must be resolved");

        assert_eq!(resource.kind(), ResourceKind::File);
        assert_eq!(
            destination.final_path(),
            Path::new("downloads").join("image.iso")
        );
    }

    #[test]
    fn progress_checkpoint_can_be_replaced() {
        let mut job = sample_job();
        let updated_at = OffsetDateTime::UNIX_EPOCH + Duration::seconds(1);
        let progress = DownloadProgress::new(512, Some(1024)).expect("test progress must be valid");

        job.update_progress(progress, updated_at);

        assert_eq!(job.progress(), progress);
        assert_eq!(job.updated_at(), updated_at);
    }

    #[test]
    fn failing_job_records_failure_and_changes_state() {
        let mut job = sample_job();
        let inspecting_at = OffsetDateTime::UNIX_EPOCH + Duration::seconds(1);
        let failed_at = OffsetDateTime::UNIX_EPOCH + Duration::seconds(2);

        job.transition_to(DownloadState::Inspecting, inspecting_at)
            .expect("Queued -> Inspecting must be valid");

        job.fail(
            DownloadFailure::new(
                FailureKind::Network,
                "connection closed before completion",
                true,
            ),
            failed_at,
        )
        .expect("Inspecting -> Failed must be valid");

        assert_eq!(job.state(), DownloadState::Failed);
        assert_eq!(job.updated_at(), failed_at);

        let failure = job.last_failure().expect("failure must be recorded");

        assert_eq!(failure.kind(), FailureKind::Network);
        assert!(failure.is_retryable());
    }
}
