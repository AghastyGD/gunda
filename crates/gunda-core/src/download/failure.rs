/// High-level category of a download failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Network,
    Authentication,
    RemoteRejected,
    InvalidResponse,
    UnsupportedResource,
    PermissionDenied,
    DiskFull,
    Integrity,
    Storage,
    Internal,
}

/// Persistable description of the latest download failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadFailure {
    kind: FailureKind,
    message: String,
    retryable: bool,
}

impl DownloadFailure {
    /// Creates a failure safe for persistence and presentation.
    ///
    /// The message must not contain cookies, authorization values, tokens,
    /// private URLs, or other sensitive request data.
    pub fn new(kind: FailureKind, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> FailureKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.retryable
    }
}

#[cfg(test)]
mod tests {
    use super::{DownloadFailure, FailureKind};

    #[test]
    fn failure_preserves_safe_domain_information() {
        let failure = DownloadFailure::new(
            FailureKind::Network,
            "connection closed before the response completed",
            true,
        );

        assert_eq!(failure.kind(), FailureKind::Network);
        assert_eq!(
            failure.message(),
            "connection closed before the response completed"
        );
        assert!(failure.is_retryable())
    }
}
