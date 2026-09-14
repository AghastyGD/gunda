mod command;
mod error;
mod event;
mod execution;
mod manager;
mod repository;

pub use command::{DownloadCommand, DownloadCommandKind};
pub use error::DownloadManagerError;
pub use event::{DownloadEvent, DownloadEventKind};
pub use execution::{
    DownloadExecutor, ExecutionInput, ExecutionOutput, ExecutionReport, PreparedTransfer,
    StagedTransfer,
};
pub use manager::DownloadManager;
pub use repository::{DownloadRepository, RepositoryError, RepositoryErrorKind};
