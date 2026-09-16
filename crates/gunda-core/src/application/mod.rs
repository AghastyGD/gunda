mod command;
mod error;
mod event;
mod execution;
mod manager;
mod repository;
mod transfer_progress;

pub use command::{DownloadCommand, DownloadCommandKind};
pub use error::DownloadManagerError;
pub use event::{DownloadEvent, DownloadEventKind};
pub use execution::{
    DownloadExecutor, ExecutionInput, ExecutionOutput, ExecutionReport, PreparedTransfer,
    StagedTransfer,
};
pub use manager::DownloadManager;
pub use repository::{DownloadRepository, RepositoryError, RepositoryErrorKind};
pub use transfer_progress::TransferProgress;
