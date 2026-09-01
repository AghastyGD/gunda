use std::error::Error;
use std::fmt;

/// Identifier of a download job persisted in the local database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownloadId(i64);

impl DownloadId {
    /// Creates an identifier from a positive database value.
    pub const fn new(value: i64) -> Result<Self, InvalidDownloadId> {
        if value > 0 {
            Ok(Self(value))
        } else {
            Err(InvalidDownloadId::new(value))
        }
    }

    /// Returns the underlying database value.
    #[must_use]
    pub const fn value(self) -> i64 {
        self.0
    }
}

impl fmt::Display for DownloadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Error returned when a database value cannot represent a download ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidDownloadId {
    value: i64,
}

impl InvalidDownloadId {
    #[must_use]
    pub const fn new(value: i64) -> Self {
        Self { value }
    }

    #[must_use]
    pub const fn value(self) -> i64 {
        self.value
    }
}

impl fmt::Display for InvalidDownloadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "download ID must be positive, received {}", self.value)
    }
}

impl Error for InvalidDownloadId {}

#[cfg(test)]
mod tests {
    use super::DownloadId;

    #[test]
    fn positive_value_creates_download_id() {
        let id = DownloadId::new(42).expect("42 is a valid download ID");

        assert_eq!(id.value(), 42);
        assert_eq!(id.to_string(), "42")
    }

    #[test]
    fn zero_and_negative_values_are_rejected() {
        for value in [0, -1, i64::MIN] {
            let error = DownloadId::new(value).expect_err("non-positive IDs must be rejected");

            assert_eq!(error.value(), value);
        }
    }
}
