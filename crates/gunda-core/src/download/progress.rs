use std::error::Error;
use std::fmt;

use crate::download::id::InvalidDownloadId;

/// Durable progress checkpoint for a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DownloadProgress {
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
}

impl DownloadProgress {
    /// Creates a validated progress checkpoint.
    pub const fn new(
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    ) -> Result<Self, InvalidDownloadProgress> {
        if let Some(total_bytes) = total_bytes
            && downloaded_bytes > total_bytes
        {
            return Err(InvalidDownloadProgress::new(downloaded_bytes, total_bytes));
        }

        Ok(Self {
            downloaded_bytes,
            total_bytes,
        })
    }

    /// Returns the number of bytes represented by this checkpoint.
    #[must_use]
    pub const fn downloaded_bytes(self) -> u64 {
        self.downloaded_bytes
    }

    /// Returns the expected total when it is known.
    #[must_use]
    pub const fn total_bytes(self) -> Option<u64> {
        self.total_bytes
    }

    /// Creates a new checkpoint with a different downloaded byte count.
    ///
    /// This does not require progress to increase because crash recovery may
    /// need to reconcile the checkpoint with a smaller partial file.
    pub const fn with_downloaded_bytes(
        self,
        downloaded_bytes: u64,
    ) -> Result<Self, InvalidDownloadProgress> {
        Self::new(downloaded_bytes, self.total_bytes)
    }

    /// Creates a new checkpoint with updated total-size information.
    pub const fn with_total_bytes(
        self,
        total_bytes: Option<u64>,
    ) -> Result<Self, InvalidDownloadProgress> {
        Self::new(self.downloaded_bytes, total_bytes)
    }

    /// Returns whether the download has reached its known total.
    ///
    /// A download with an unknown total is never considered completed through
    /// progress alone. Finalization still controls the `Completed` state.
    #[must_use]
    pub const fn has_reached_known_total(self) -> bool {
        matches!(
            self.total_bytes,
            Some(total_bytes) if self.downloaded_bytes == total_bytes
        )
    }
}

/// Error returned when download bytes exceed a known total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidDownloadProgress {
    downloaded_bytes: u64,
    total_bytes: u64,
}

impl InvalidDownloadProgress {
    #[must_use]
    pub const fn new(downloaded_bytes: u64, total_bytes: u64) -> Self {
        Self {
            downloaded_bytes,
            total_bytes,
        }
    }

    #[must_use]
    pub const fn downloaded_bytes(self) -> u64 {
        self.downloaded_bytes
    }

    #[must_use]
    pub const fn total_bytes(self) -> u64 {
        self.total_bytes
    }
}

impl fmt::Display for InvalidDownloadProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "downloaded bytes ({}) exceed total bytes ({})",
            self.downloaded_bytes, self.total_bytes
        )
    }
}

impl Error for InvalidDownloadProgress {}

#[cfg(test)]
mod tests {
    use super::DownloadProgress;

    #[test]
    fn default_progress_starts_at_zero_with_unknown_total() {
        let progress = DownloadProgress::default();

        assert_eq!(progress.downloaded_bytes(), 0);
        assert_eq!(progress.total_bytes(), None);
        assert!(!progress.has_reached_known_total());
    }

    #[test]
    fn progress_accepts_downloaded_bytes_within_known_total() {
        let progress = DownloadProgress::new(512, Some(1024)).expect("progress must be valid");

        assert_eq!(progress.downloaded_bytes(), 512);
        assert_eq!(progress.total_bytes(), Some(1024));
        assert!(!progress.has_reached_known_total());
    }

    #[test]
    fn progress_rejects_downloaded_bytes_above_known_total() {
        let error = DownloadProgress::new(1025, Some(1024))
            .expect_err("downloaded byte above total must be rejected");

        assert_eq!(error.downloaded_bytes(), 1025);
        assert_eq!(error.total_bytes(), 1024);
        assert_eq!(
            error.to_string(),
            "downloaded bytes (1025) exceed total bytes (1024)"
        );
    }

    #[test]
    fn changing_total_revalidates_existing_progress() {
        let progress =
            DownloadProgress::new(512, None).expect("unknown total allows the checkpoint");

        assert!(progress.with_total_bytes(Some(1024)).is_ok());
        assert!(progress.with_total_bytes(Some(100)).is_err());
    }

    #[test]
    fn reaching_total_does_not_require_positive_file_size() {
        let empty =
            DownloadProgress::new(0, Some(0)).expect("an empty resource has valid progress");

        assert!(empty.has_reached_known_total());
    }
}
