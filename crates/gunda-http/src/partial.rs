use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use gunda_core::download::{DownloadId, RequestContext};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use crate::{HttpClient, HttpError, HttpInspection};

/// File operation that failed withour exposing the affected path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    Create,
    Write,
    Flush,
    Sync,
}

/// Failure while transferring HTTP content into a partial file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialDownloadError {
    Http(HttpError),

    File {
        operation: FileOperation,
        kind: io::ErrorKind,
    },

    ByteCountOverflow,
}

impl fmt::Display for PartialDownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => error.fmt(f),
            Self::File { operation, kind } => {
                write!(f, "partial file operation failed: {operation:?} ({kind:?})")
            }
            Self::ByteCountOverflow => {
                f.write_str("partial file byte count exceeds the supported range")
            }
        }
    }
}

impl Error for PartialDownloadError {}

impl From<HttpError> for PartialDownloadError {
    fn from(error: HttpError) -> Self {
        Self::Http(error)
    }
}

/// A fully received body stored in a partial file.
///
/// This is not a finalized destination or a completed download job.
/// It intentionally does not implement Debug.
pub struct PartialDownload {
    id: DownloadId,
    path: PathBuf,
    written_bytes: u64,
    metadata: HttpInspection,
}

impl PartialDownload {
    #[must_use]
    pub fn id(&self) -> DownloadId {
        self.id
    }
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn written_bytes(&self) -> u64 {
        self.written_bytes
    }

    #[must_use]
    pub const fn metadata(&self) -> &HttpInspection {
        &self.metadata
    }
}

/// Computes the initiual staging path using only a local download identifier.
///
/// The directory is selected by the application, not by a remote filename.
#[must_use]
pub fn partial_path(directory: &Path, id: DownloadId) -> PathBuf {
    directory.join(format!(".gunda-{}.part", id.value()))
}

/// Downlaods a full HTTP response into an exclusively created partial file.
///
/// The directory must already exist and be controlled by the application/user.
/// Existing paths are never overwritten. Failures preserve any partial file
/// already created; this function does not implement cleanup or resume
///
/// Success requires a valid HTTP EOF, completed writes, and successful file
/// synchronization. It does not rename the file or update persistent job state.
pub async fn download_to_partial(
    client: &HttpClient,
    request: &RequestContext,
    id: DownloadId,
    directory: &Path,
) -> Result<PartialDownload, PartialDownloadError> {
    let mut body = client.open(request).await?; // TODO: should we check/create the partial before opening the GET?
    let metadata = body.metadata().clone();
    let path = partial_path(directory, id);

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
        .map_err(|error| file_error(FileOperation::Create, error))?;

    let mut written_bytes = 0_u64;

    while let Some(chunk) = body.next_chunk().await? {
        let size =
            u64::try_from(chunk.len()).map_err(|_| PartialDownloadError::ByteCountOverflow)?;

        let next_written_bytes = written_bytes
            .checked_add(size)
            .ok_or(PartialDownloadError::ByteCountOverflow)?;

        file.write_all(&chunk)
            .await
            .map_err(|error| file_error(FileOperation::Write, error))?;

        // Tokio file writes may still be pending after write_all returns.
        // Flush waits for them before advancing our written-byte counter.
        file.flush()
            .await
            .map_err(|error| file_error(FileOperation::Flush, error))?;

        written_bytes = next_written_bytes;
    }

    file.sync_all()
        .await
        .map_err(|error| file_error(FileOperation::Sync, error))?;

    drop(file);

    Ok(PartialDownload {
        id,
        path,
        written_bytes,
        metadata,
    })
}

fn file_error(operation: FileOperation, error: io::Error) -> PartialDownloadError {
    PartialDownloadError::File {
        operation,
        kind: error.kind(),
    }
}
