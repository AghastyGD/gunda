use std::error::Error;
use std::fmt;

use super::{DownloadCommandKind, RepositoryError};
use crate::download::{DownloadId, DownloadState, ResolvedDestination};

/// Failure while handling a download management operation.
#[derive(Clone, PartialEq, Eq)]
pub enum DownloadManagerError {
    NotFound {
        id: DownloadId,
    },

    InvalidOperation {
        id: DownloadId,
        command: DownloadCommandKind,
        state: DownloadState,
    },

    Repository(RepositoryError),
    InvalidExecutionState {
        id: DownloadId,
        state: DownloadState,
    },

    CompletionNotPersisted {
        id: DownloadId,
        destination: ResolvedDestination,
        written_bytes: u64,
        cleanup_pending: bool,
        error: RepositoryError,
    },
}

impl fmt::Display for DownloadManagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { .. } => f.write_str("download does not exist"),
            Self::InvalidOperation { .. } => {
                f.write_str("operation is not supported in the current download state")
            }
            Self::Repository(_) => f.write_str("download persistence operation failed"),
            Self::InvalidExecutionState { .. } => {
                f.write_str("download must be queued before execution")
            }
            Self::CompletionNotPersisted { .. } => {
                f.write_str("output was published but completion was not persisted")
            }
        }
    }
}

impl fmt::Debug for DownloadManagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DownloadManagerError: ")?;
        fmt::Display::fmt(self, f)
    }
}

impl Error for DownloadManagerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Repository(error) => Some(error),
            Self::CompletionNotPersisted { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl From<RepositoryError> for DownloadManagerError {
    fn from(error: RepositoryError) -> Self {
        Self::Repository(error)
    }
}
