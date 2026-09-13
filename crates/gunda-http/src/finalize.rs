use std::error::Error;
use std::fmt;
use std::io;
use std::path::Path;

use gunda_core::download::{DownloadDestination, FileConflictPolicy, ResolvedDestination};

use crate::PartialDownload;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeError {
    InvalidFilename,
    DifferentDirectory,
    InvalidPartial,
    NameAttemptsExhausted,
    File(io::ErrorKind),
}

impl fmt::Display for FinalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFilename => f.write_str("destination filename is invalid or reserved"),
            Self::DifferentDirectory => {
                f.write_str("partial and final output must use the same directory")
            }
            Self::InvalidPartial => {
                f.write_str("partial file no longer matches the transfer result")
            }
            Self::NameAttemptsExhausted => {
                f.write_str("could not find an available destination name")
            }
            Self::File(kind) => {
                write!(f, "output finalization failed: {kind:?}")
            }
        }
    }
}

impl Error for FinalizeError {}

/// An unsuccessful finalization retains ownership of the partial result.
///
/// Debug intentionally excludes paths and response metadata.
pub struct FinalizeFailure {
    partial: PartialDownload,
    error: FinalizeError,
}

impl FinalizeFailure {
    #[must_use]
    pub const fn error(&self) -> FinalizeError {
        self.error
    }

    #[must_use]
    pub fn into_parts(self) -> (PartialDownload, FinalizeError) {
        (self.partial, self.error)
    }
}

impl fmt::Debug for FinalizeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FinalizeFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for FinalizeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl Error for FinalizeFailure {}

/// Output published under its final name.
///
/// This does not mean the corresponding job has been committed as Completed.
pub struct FinalizedDownload {
    destination: ResolvedDestination,
    written_bytes: u64,
    partial_cleanup_error: Option<io::ErrorKind>,
}

impl FinalizedDownload {
    #[must_use]
    pub const fn destination(&self) -> &ResolvedDestination {
        &self.destination
    }

    #[must_use]
    pub const fn written_bytes(&self) -> u64 {
        self.written_bytes
    }

    /// A cleanup failure does not undo successful publication.
    #[must_use]
    pub const fn partial_cleanup_error(&self) -> Option<io::ErrorKind> {
        self.partial_cleanup_error
    }
}

/// Publishes a completed partial file under a validated local filename.
///
/// The destination directory must be the same path used for staging and must
/// not be modified by untrusted processes during this operation.
///
/// No database state is changed. Directory synchronization and crash recovery
/// are not implemented here.
pub async fn finalize_download(
    partial: PartialDownload,
    destination: &DownloadDestination,
) -> Result<FinalizedDownload, FinalizeFailure> {
    match publish(&partial, destination).await {
        Ok(finalized) => Ok(finalized),
        Err(error) => Err(FinalizeFailure { partial, error }),
    }
}

async fn publish(
    partial: &PartialDownload,
    destination: &DownloadDestination,
) -> Result<FinalizedDownload, FinalizeError> {
    if partial.path().parent() != Some(destination.directory()) {
        return Err(FinalizeError::DifferentDirectory);
    }

    let filename = match destination.preferred_filename() {
        Some(name) => name.to_owned(),
        None => format!("download-{}.bin", partial.id().value()),
    };

    validate_filename(&filename)?;

    let metadata = tokio::fs::symlink_metadata(partial.path())
        .await
        .map_err(file_error)?;

    if !metadata.file_type().is_file() || metadata.len() != partial.written_bytes() {
        return Err(FinalizeError::InvalidPartial);
    }

    if destination.conflict_policy() == FileConflictPolicy::Overwrite {
        let final_path = destination.directory().join(&filename);

        tokio::fs::rename(partial.path(), &final_path)
            .await
            .map_err(file_error)?;

        return Ok(FinalizedDownload {
            destination: ResolvedDestination::new(final_path),
            written_bytes: partial.written_bytes(),
            partial_cleanup_error: None,
        });
    }

    let attempts = match destination.conflict_policy() {
        FileConflictPolicy::Fail => 1,
        FileConflictPolicy::Rename => 1000,
        FileConflictPolicy::Overwrite => unreachable!(),
    };

    for attempt in 0..attempts {
        let candidate = candidate_name(&filename, attempt);
        let final_path = destination.directory().join(candidate);

        match tokio::fs::hard_link(partial.path(), &final_path).await {
            Ok(()) => {
                // Publication already succeeded. Failure to remove the staging
                // name must not be reported as a failed publication.
                let partial_cleanup_error = tokio::fs::remove_file(partial.path())
                    .await
                    .err()
                    .map(|error| error.kind());

                return Ok(FinalizedDownload {
                    destination: ResolvedDestination::new(final_path),
                    written_bytes: partial.written_bytes(),
                    partial_cleanup_error,
                });
            }
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    && destination.conflict_policy() == FileConflictPolicy::Rename =>
            {
                continue;
            }
            Err(error) => return Err(file_error(error)),
        }
    }

    Err(FinalizeError::NameAttemptsExhausted)
}

fn candidate_name(filename: &str, attempt: u32) -> String {
    if attempt == 0 {
        return filename.to_owned();
    }

    match filename.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => {
            format!("{stem} ({attempt}).{extension}")
        }
        _ => format!("{filename} ({attempt})"),
    }
}

fn file_error(error: io::Error) -> FinalizeError {
    FinalizeError::File(error.kind())
}

fn validate_filename(filename: &str) -> Result<(), FinalizeError> {
    // Conservative application limit, leaving room for rename suffixes.
    if filename.is_empty()
        || filename.len() > 200
        || matches!(filename, "." | "..")
        || filename.ends_with([' ', '.'])
        || filename.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '~'
                )
        })
    {
        return Err(FinalizeError::InvalidFilename);
    }

    let uppercase = filename.to_ascii_uppercase();

    // Keep final names out of the staging namespace.
    if uppercase.starts_with(".GUNDA-") {
        return Err(FinalizeError::InvalidFilename);
    }

    let base = uppercase
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');

    if matches!(base, "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return Err(FinalizeError::InvalidFilename);
    }

    for prefix in ["COM", "LPT"] {
        if let Some(suffix) = base.strip_prefix(prefix)
            && matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        {
            return Err(FinalizeError::InvalidFilename);
        }
    }

    // Defense in depth: the filename must remain one path component.
    if Path::new(filename).components().count() != 1 {
        return Err(FinalizeError::InvalidFilename);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{candidate_name, validate_filename};

    #[test]
    fn unsafe_and_reserved_names_are_rejected() {
        for filename in [
            "",
            ".",
            "..",
            "../escape.bin",
            r"..\escape.bin",
            "/absolute.bin",
            r"C:\file.bin",
            "file:stream",
            "file.",
            "file ",
            "NUL.txt",
            "con",
            "COM1.bin",
            "LPT²",
            ".gunda-56.part",
            "GUNDA~1.PAR",
        ] {
            assert!(validate_filename(filename).is_err());
        }

        assert!(validate_filename(&"a".repeat(201)).is_err());
    }

    #[test]
    fn ordinary_unicode_names_are_accepted() {
        for filename in ["image.iso", "relatório.pdf", ".config", "video final.mp4"] {
            assert!(validate_filename(filename).is_ok());
        }
    }

    #[test]
    fn rename_suffix_preserves_the_last_extension() {
        assert_eq!(candidate_name("image.iso", 0), "image.iso");
        assert_eq!(candidate_name("image.iso", 1), "image (1).iso");
        assert_eq!(candidate_name("archive.tar.gz", 2), "archive.tar (2).gz");
        assert_eq!(candidate_name(".config", 1), ".config (1)");
    }
}
