use std::future::Future;

use crate::download::{
    DownloadDestination, DownloadFailure, DownloadId, RequestContext, ResolvedDestination,
    ResourceDescriptor,
};

use super::DownloadEvent;

/// Input for a single execution attempt
pub struct ExecutionInput {
    pub id: DownloadId,
    pub request: RequestContext,
    pub destination: DownloadDestination,
}

/// Opens a resource and prepares it for transfer.
pub trait DownloadExecutor: Send + Sync {
    type Prepared: PreparedTransfer;

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

    /// Returns the output path planned for this executino.
    fn planned_destination(&self) -> ResolvedDestination;

    /// Consumes the prepared resource and writes its staging output.
    fn transfer(self) -> impl Future<Output = Result<Self::Staged, DownloadFailure>> + Send;
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
