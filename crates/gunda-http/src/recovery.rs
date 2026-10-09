use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::Path;

use gunda_core::download::{DownloadJob, DownloadState, ResourceKind};

use crate::partial_path;

/// Filesystem metadata observed at a recorvery path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalFileState {
    Missing,
    RegularFile { bytes: u64 },
    Symlink,
    Other,
    Unavailable { kind: io::ErrorKind },
}

/// Compares partial-file length with the persisted progress checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointComparison {
    Unavailable,
    FileShorter,
    SameLength,
    FileLonger,
}

/// Local observations for an interrupted direct HTTP download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRecoveryInspection {
    pub checkpoint_bytes: u64,
    pub total_bytes: Option<u64>,
    pub partial: LocalFileState,
    pub planned_output: Option<LocalFileState>,
}

impl HttpRecoveryInspection {
    #[must_use]
    pub fn checkpoint_comparison(&self) -> CheckpointComparison {
        let LocalFileState::RegularFile { bytes } = self.partial else {
            return CheckpointComparison::Unavailable;
        };

        match bytes.cmp(&self.checkpoint_bytes) {
            Ordering::Less => CheckpointComparison::FileShorter,
            Ordering::Equal => CheckpointComparison::SameLength,
            Ordering::Greater => CheckpointComparison::FileLonger,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryInspectionError {
    NotInterrupted,
    UnsupportedResource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeFileError {
    MissingPartial,
    PartialTooShort,
    UnsafePartial,
    PartialUnavailable(io::ErrorKind),
    OutputExists,
    OutputUnavailable(io::ErrorKind),
}

impl fmt::Display for RecoveryInspectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInterrupted => {
                f.write_str("local recovery inspection requires an interrupted download")
            }
            Self::UnsupportedResource => {
                f.write_str("local recovery inspection requires a direct HTTP resource")
            }
        }
    }
}

impl Error for RecoveryInspectionError {}

/// Observes local recovery paths without underlying files or persisted state.
pub async fn inspect_local_recovery(
    job: &DownloadJob,
) -> Result<HttpRecoveryInspection, RecoveryInspectionError> {
    if job.state() != DownloadState::Interrupted {
        return Err(RecoveryInspectionError::NotInterrupted);
    }

    if !matches!(job.request().url().scheme(), "http" | "https")
        || job
            .resource()
            .is_some_and(|resource| resource.kind() != ResourceKind::File)
    {
        return Err(RecoveryInspectionError::UnsupportedResource);
    }

    let staging_path = partial_path(job.destination().directory(), job.id());
    let partial = inspect_path(&staging_path).await;

    let planned_output = match job.resolved_destination() {
        Some(destination) => Some(inspect_path(destination.final_path()).await),
        None => None,
    };

    Ok(HttpRecoveryInspection {
        checkpoint_bytes: job.progress().downloaded_bytes(),
        total_bytes: job.progress().total_bytes(),
        partial,
        planned_output,
    })
}

pub(crate) async fn inspect_resume_candidate(
    partial_path: &Path,
    planned_output: Option<&Path>,
    durable_bytes: u64,
) -> Result<u64, ResumeFileError> {
    if let Some(path) = planned_output {
        match inspect_path(path).await {
            LocalFileState::Missing => {}
            LocalFileState::Unavailable { kind } => {
                return Err(ResumeFileError::OutputUnavailable(kind));
            }
            _ => return Err(ResumeFileError::OutputExists),
        }
    }

    match inspect_path(partial_path).await {
        LocalFileState::RegularFile { bytes } if bytes >= durable_bytes => Ok(bytes),
        LocalFileState::RegularFile { .. } => Err(ResumeFileError::PartialTooShort),
        LocalFileState::Missing => Err(ResumeFileError::MissingPartial),
        LocalFileState::Unavailable { kind } => Err(ResumeFileError::PartialUnavailable(kind)),
        LocalFileState::Symlink | LocalFileState::Other => Err(ResumeFileError::UnsafePartial),
    }
}

pub(crate) async fn inspect_path(path: &Path) -> LocalFileState {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => {
            let file_type = metadata.file_type();

            if file_type.is_symlink() {
                LocalFileState::Symlink
            } else if file_type.is_file() {
                LocalFileState::RegularFile {
                    bytes: metadata.len(),
                }
            } else {
                LocalFileState::Other
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => LocalFileState::Missing,
        Err(error) => LocalFileState::Unavailable { kind: error.kind() },
    }
}
