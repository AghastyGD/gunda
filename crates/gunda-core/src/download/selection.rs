use std::error::Error;
use std::fmt;

/// Opaque selector for one track within a presentation.
///
/// The engine that executes the request owns the meaning. Values contain only
/// short ASCII identifiers and numbers, never URLs, credentials, or manifest
/// text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionSelection {
    value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidExecutionSelection;

impl ExecutionSelection {
    /// Largest selection accepted for durable storage.
    pub const MAX_LEN: usize = 128;

    pub fn new(value: impl Into<String>) -> Result<Self, InvalidExecutionSelection> {
        let value = value.into();

        if value.is_empty() || value.len() > Self::MAX_LEN {
            return Err(InvalidExecutionSelection);
        }

        if !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.' | b':' | b';' | b',' | b'=')
        }) {
            return Err(InvalidExecutionSelection);
        }

        Ok(Self { value })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for ExecutionSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

impl fmt::Display for InvalidExecutionSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "the execution selection is empty, too long, or contains unsupported characters",
        )
    }
}

impl Error for InvalidExecutionSelection {}

#[cfg(test)]
mod tests {
    use super::{ExecutionSelection, InvalidExecutionSelection};

    #[test]
    fn accepts_compact_engine_identifiers() {
        let selection = ExecutionSelection::new("hls:1:3:1000000:1920:1080").unwrap();

        assert_eq!(selection.as_str(), "hls:1:3:1000000:1920:1080");
    }

    #[test]
    fn refuses_urls_and_secret_shaped_values() {
        for value in [
            "https://cdn.example/video.m3u8?token=secret",
            "hls:1:3 with spaces",
            "",
        ] {
            assert_eq!(
                ExecutionSelection::new(value),
                Err(InvalidExecutionSelection)
            );
        }
    }

    #[test]
    fn refuses_values_beyond_the_storage_limit() {
        let value = "a".repeat(ExecutionSelection::MAX_LEN + 1);

        assert_eq!(
            ExecutionSelection::new(value),
            Err(InvalidExecutionSelection)
        );
    }
}
