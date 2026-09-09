use std::error::Error;
use std::fmt;

use super::{DownloadCommandKind, RepositoryError};
use crate::download::{DownloadId, DownloadState};

/// Failure while handling a download management operation.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl fmt::Display for DownloadManagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { .. } => f.write_str("download does not exist"),
            Self::InvalidOperation { .. } => {
                f.write_str("operation is not supported in the current download state")
            }
            Self::Repository(_) => f.write_str("download persistence operation failed"),
        }
    }
}

impl Error for DownloadManagerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Repository(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RepositoryError> for DownloadManagerError {
    fn from(error: RepositoryError) -> Self {
        Self::Repository(error)
    }
}
