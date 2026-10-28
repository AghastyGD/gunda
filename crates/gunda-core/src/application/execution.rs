use std::future::Future;

use crate::download::{
    DownloadDestination, DownloadFailure, DownloadId, DownloadProgress, RequestContext,
    ResolvedDestination, ResourceDescriptor,
};

use super::{DownloadCancellation, DownloadEvent, TransferProgress};
/// Input for a single execution attempt
pub struct ExecutionInput {
    pub id: DownloadId,
    pub request: RequestContext,
    pub destination: DownloadDestination,
    pub progress: DownloadProgress,
    pub resource: Option<ResourceDescriptor>,
    pub resolved_destination: Option<ResolvedDestination>,
}

/// Outcome of writing an execution's staging file.
pub enum TransferOutcome<T> {
    Finished(T),
    Stopped { written_bytes: u64 },
}

/// Opens a resource and prepares it for transfer.
pub trait DownloadExecutor: Send + Sync {
    type Prepared: PreparedTransfer;

    /// Prepares a transfer; dropping this future must safely release its resources.
    fn prepare(
        &self,
        input: ExecutionInput,
    ) -> impl Future<Output = Result<Self::Prepared, DownloadFailure>> + Send;
}

/// An opened resource whose body has not been written to staging.
pub trait PreparedTransfer: Send + Sized {
    type Staged: StagedTransfer;

    fn resource(&self) -> ResourceDescriptor;

    fn total_bytes(&self) -> Option<u64>;

    /// Whether the total is derived rather than measured from every resource.
    fn total_bytes_is_estimated(&self) -> bool {
        false
    }

    /// Returns the reconciled byte count already present in staging.
    fn starting_bytes(&self) -> u64 {
        0
    }

    /// Returns whether this prepared transfer can stop at a durable checkpoint.
    fn supports_pause(&self) -> bool {
        false
    }

    /// Whether a stopped transfer may report fewer bytes than it last reported.
    ///
    /// A segmented executor discards an incomplete resource and resumes from the
    /// preceding durable boundary, so its stopped count can move backwards.
    fn stopped_progress_may_rewind(&self) -> bool {
        false
    }

    /// Returns the output path planned for this executino.
    fn planned_destination(&self) -> ResolvedDestination;

    /// Writes staging output without progress observation.
    fn transfer(
        self,
    ) -> impl Future<Output = Result<TransferOutcome<Self::Staged>, DownloadFailure>> + Send {
        self.transfer_with_progress(TransferProgress::disabled())
    }

    /// Writes staging output and reports cumulative written bytes.
    fn transfer_with_progress(
        self,
        progress: TransferProgress,
    ) -> impl Future<Output = Result<TransferOutcome<Self::Staged>, DownloadFailure>> + Send {
        self.transfer_controlled(progress, DownloadCancellation::new())
    }

    fn transfer_controlled(
        self,
        progress: TransferProgress,
        cancellation: DownloadCancellation,
    ) -> impl Future<Output = Result<TransferOutcome<Self::Staged>, DownloadFailure>> + Send;
}

/// Fully transferred stagin output that has not been published
pub trait StagedTransfer: Send + Sized {
    fn written_bytes(&self) -> u64;

    /// Consumes staging output and publishes its final destination.
    fn finalize(self) -> impl Future<Output = Result<ExecutionOutput, DownloadFailure>> + Send;
}

/// Successful filesystem publication, not a persisted Completed job.
pub struct ExecutionOutput {
    pub destination: ResolvedDestination,
    pub written_bytes: u64,
    pub cleanup_pending: bool,
}

/// Terminal result of an execution coordinated by the manager
pub struct ExecutionReport {
    pub event: DownloadEvent,
    pub cleanup_pending: bool,
}
