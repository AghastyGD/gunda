use std::path::{Path, PathBuf};

/// Policy applied when the selected final path already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileConflictPolicy {
    Rename,
    Overwrite,
    Fail,
}

/// User intent for the destination of a download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadDestination {
    directory: PathBuf,
    preferred_filename: Option<String>,
    conflict_policy: FileConflictPolicy,
}

impl DownloadDestination {
    #[must_use]
    pub fn new(
        directory: PathBuf,
        preferred_filename: Option<String>,
        conflict_policy: FileConflictPolicy,
    ) -> Self {
        Self {
            directory,
            preferred_filename,
            conflict_policy,
        }
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[must_use]
    pub fn preferred_filename(&self) -> Option<&str> {
        self.preferred_filename.as_deref()
    }

    #[must_use]
    pub const fn conflict_policy(&self) -> FileConflictPolicy {
        self.conflict_policy
    }
}

/// Final destination selected after inspection and conflict resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDestination {
    final_path: PathBuf,
}

impl ResolvedDestination {
    #[must_use]
    pub fn new(final_path: PathBuf) -> Self {
        Self { final_path }
    }

    #[must_use]
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }
}

#[cfg(test)]
mod tests {
    use super::{DownloadDestination, FileConflictPolicy};
    use std::path::{Path, PathBuf};

    #[test]
    fn destination_preserves_native_path_without_string_conversion() {
        let destination = DownloadDestination::new(
            PathBuf::from("downloads").join("linux"),
            Some("image.iso".to_owned()),
            FileConflictPolicy::Rename,
        );

        assert_eq!(
            destination.directory(),
            Path::new("downloads").join("linux")
        );
        assert_eq!(destination.preferred_filename(), Some("image.iso"));
        assert_eq!(destination.conflict_policy(), FileConflictPolicy::Rename);
    }
}
