use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::Path;

use gunda_core::download::{DownloadJob, DownloadState, ResourceKind};
use same_file::Handle;
use tokio::fs::{File, OpenOptions};

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

pub(crate) struct ResumePartial {
    file: File,
    identity: Handle,
}

impl ResumePartial {
    pub(crate) fn into_parts(self) -> (File, Handle) {
        (self.file, self.identity)
    }
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
) -> Result<ResumePartial, ResumeFileError> {
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
        LocalFileState::RegularFile { bytes } if bytes >= durable_bytes => Ok(()),
        LocalFileState::RegularFile { .. } => Err(ResumeFileError::PartialTooShort),
        LocalFileState::Missing => Err(ResumeFileError::MissingPartial),
        LocalFileState::Unavailable { kind } => Err(ResumeFileError::PartialUnavailable(kind)),
        LocalFileState::Symlink | LocalFileState::Other => Err(ResumeFileError::UnsafePartial),
    }?;

    let mut options = OpenOptions::new();
    options.read(true).write(true);
    configure_no_follow(&mut options);

    let file = match options.open(partial_path).await {
        Ok(file) => file,
        Err(error) => {
            return Err(match inspect_path(partial_path).await {
                LocalFileState::Missing => ResumeFileError::MissingPartial,
                LocalFileState::Symlink | LocalFileState::Other => ResumeFileError::UnsafePartial,
                _ => ResumeFileError::PartialUnavailable(error.kind()),
            });
        }
    };

    let metadata = file
        .metadata()
        .await
        .map_err(|error| ResumeFileError::PartialUnavailable(error.kind()))?;

    if !metadata.is_file() {
        return Err(ResumeFileError::UnsafePartial);
    }

    if metadata.len() < durable_bytes {
        return Err(ResumeFileError::PartialTooShort);
    }

    let identity = file_identity(&file)
        .await
        .map_err(|error| ResumeFileError::PartialUnavailable(error.kind()))?;

    match path_matches_identity(partial_path, &identity).await {
        Ok(true) => Ok(ResumePartial { file, identity }),
        Ok(false) => Err(ResumeFileError::UnsafePartial),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(ResumeFileError::MissingPartial)
        }
        Err(error) => Err(ResumeFileError::PartialUnavailable(error.kind())),
    }
}

pub(crate) async fn file_identity(file: &File) -> io::Result<Handle> {
    let clone = file.try_clone().await?.into_std().await;
    tokio::task::spawn_blocking(move || Handle::from_file(clone))
        .await
        .map_err(io::Error::other)?
}

pub(crate) async fn path_matches_identity(path: &Path, identity: &Handle) -> io::Result<bool> {
    if !tokio::fs::symlink_metadata(path)
        .await?
        .file_type()
        .is_file()
    {
        return Ok(false);
    }

    let path = path.to_owned();
    let path_identity = tokio::task::spawn_blocking(move || Handle::from_path(path))
        .await
        .map_err(io::Error::other)??;

    Ok(path_identity == *identity)
}

#[cfg(target_os = "linux")]
fn configure_no_follow(options: &mut OpenOptions) {
    const O_NOFOLLOW: i32 = 0x20_000;
    options.custom_flags(O_NOFOLLOW);
}

#[cfg(windows)]
fn configure_no_follow(options: &mut OpenOptions) {
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(target_os = "linux", windows)))]
fn configure_no_follow(_options: &mut OpenOptions) {}

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
