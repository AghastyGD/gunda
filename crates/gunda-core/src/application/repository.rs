use std::fmt;
use std::future::Future;
use std::{error::Error, fmt::Formatter};

use time::OffsetDateTime;

use crate::download::{DownloadId, DownloadJob, NewDownload};

/// Storage operations required by the application layer.
///
/// The trait uses return-position `impl Future` instead of `async fn` so that
/// the returned future is explicitly required to be `Send`.
pub trait DownloadRepository: Send + Sync {
    /// Persists a new download before it becomes eligible for execution.
    fn create(
        &self,
        download: NewDownload,
        created_at: OffsetDateTime,
    ) -> impl Future<Output = Result<DownloadJob, RepositoryError>> + Send;

    /// Loads a download by its local identifier.
    fn find_by_id(
        &self,
        id: DownloadId,
    ) -> impl Future<Output = Result<Option<DownloadJob>, RepositoryError>> + Send;

    /// Loads all persisted downloads in ascending identifier order.
    ///
    /// Returning a deterministic order keeps startup snapshots stable across
    /// repository implementations.
    fn list(&self) -> impl Future<Output = Result<Vec<DownloadJob>, RepositoryError>> + Send;
}

/// Stable classification of persistence failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryErrorKind {
    /// The storage resource could not be opened or reached.
    Unavailable,

    /// Persisted data does not satisfy the domain contract.
    InvalidData,

    /// A database or domain constraint was violated.
    ConstraintViolation,

    /// The operation requires secret storage that is not implemented.
    SensitiveDataUnsupported,

    /// An unexpected persistence operation failed.
    Internal,
}

/// Persistence error safe for the application and presentation layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryError {
    kind: RepositoryErrorKind,
    message: String,
}

impl RepositoryError {
    /// Creates an error with a safe diagnostic message.
    ///
    /// The message must not contain SQL parameters, URLs, paths, headers,
    /// credentials, or other user-provided values.
    #[must_use]
    pub fn new(kind: RepositoryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> RepositoryErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for RepositoryError {}

#[cfg(test)]
mod tests {
    use super::{RepositoryError, RepositoryErrorKind};

    #[test]
    fn repository_error_preserves_safe_context() {
        let error = RepositoryError::new(
            RepositoryErrorKind::Unavailable,
            "could not open the download database",
        );

        assert_eq!(error.kind(), RepositoryErrorKind::Unavailable);
        assert_eq!(error.message(), "could not open the download database");
        assert_eq!(error.to_string(), "could not open the download database");
    }
}
