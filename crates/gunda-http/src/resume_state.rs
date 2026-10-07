use std::error::Error;
use std::fmt;
use std::future::Future;

use gunda_core::application::RepositoryError;
use gunda_core::download::DownloadId;

use crate::StrongEntityTag;

/// Representation identity and synchronized partial-file checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResumeState {
    strong_etag: StrongEntityTag,
    total_bytes: u64,
    durable_bytes: u64,
}

impl HttpResumeState {
    pub fn new(
        strong_etag: StrongEntityTag,
        total_bytes: u64,
        durable_bytes: u64,
    ) -> Result<Self, InvalidHttpResumeState> {
        if durable_bytes > total_bytes {
            return Err(InvalidHttpResumeState);
        }

        Ok(Self {
            strong_etag,
            total_bytes,
            durable_bytes,
        })
    }

    #[must_use]
    pub const fn strong_etag(&self) -> &StrongEntityTag {
        &self.strong_etag
    }

    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    #[must_use]
    pub const fn durable_bytes(&self) -> u64 {
        self.durable_bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidHttpResumeState;

impl fmt::Display for InvalidHttpResumeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HTTP durable checkpoint exceeds the representation size")
    }
}

impl Error for InvalidHttpResumeState {}

/// Persists representation identity and durable HTTP checkpoints.
pub trait HttpResumeStore: Send + Sync {
    fn initialize(
        &self,
        id: DownloadId,
        strong_etag: &StrongEntityTag,
        total_bytes: u64,
    ) -> impl Future<Output = Result<HttpResumeState, RepositoryError>> + Send;

    fn find(
        &self,
        id: DownloadId,
    ) -> impl Future<Output = Result<HttpResumeState, RepositoryError>> + Send;

    /// Advances a checkpoint after the caller synchronizes the partial file.
    fn save_checkpoint(
        &self,
        id: DownloadId,
        state: &HttpResumeState,
    ) -> impl Future<Output = Result<HttpResumeState, RepositoryError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::HttpResumeState;
    use crate::StrongEntityTag;

    #[test]
    fn checkpoint_cannot_exceed_the_representation_size() {
        let tag = StrongEntityTag::parse(b"\"version-23\"").expect("tag must be valid");

        assert!(HttpResumeState::new(tag, 10, 11).is_err());
    }

    #[test]
    fn diagnostics_do_not_expose_the_representation_tag() {
        let tag =
            StrongEntityTag::parse(b"\"private-validator-marker\"").expect("tag must be valid");

        let state = HttpResumeState::new(tag, 10, 5).expect("state must be valid");

        assert!(!format!("{state:?}").contains("private-validator-marker"));
    }
}
