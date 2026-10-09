use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gunda_core::application::RepositoryErrorKind;
use gunda_core::application::{DownloadCancellation, TransferOutcome, TransferProgress};
use gunda_core::download::{DownloadId, RequestContext};
use tokio::fs::File;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use crate::HttpResumeState;
use crate::resume_state::ResumePersistence;
use crate::{HttpBody, HttpClient, HttpError, HttpInspection, HttpRangeBody};

/// File operation that failed withour exposing the affected path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    Create,
    Open,
    Write,
    Flush,
    Sync,
    Truncate,
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
    InvalidBodyState,
    InvalidPartial,
    ResumeStore(RepositoryErrorKind),
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
            Self::InvalidBodyState => {
                f.write_str("HTTP body is no longer available for a complete transfer")
            }
            Self::InvalidPartial => {
                f.write_str("partial file does not match the durable HTTP checkpoint")
            }
            Self::ResumeStore(kind) => {
                write!(f, "HTTP resume storage failed: {kind:?}")
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
) -> Result<TransferOutcome<PartialDownload>, PartialDownloadError> {
    let body = client.open(request).await?; // TODO: should we reserve the partial file before opening the GET?

    write_body_to_partial(
        body,
        id,
        directory,
        TransferProgress::disabled(),
        DownloadCancellation::new(),
    )
    .await
}

/// Writes an already opened body into an exclusively created partial file.
///
/// The body must not have been consumed or failed.
/// Existing files are never overwritten.
pub(crate) async fn write_body_to_partial(
    body: HttpBody,
    id: DownloadId,
    directory: &Path,
    progress: TransferProgress,
    cancellation: DownloadCancellation,
) -> Result<TransferOutcome<PartialDownload>, PartialDownloadError> {
    write_body_to_partial_with_resume(body, id, directory, progress, cancellation, None).await
}
pub(crate) async fn write_body_to_partial_with_resume(
    body: HttpBody,
    id: DownloadId,
    directory: &Path,
    progress: TransferProgress,
    cancellation: DownloadCancellation,
    resume_store: Option<Arc<dyn ResumePersistence>>,
) -> Result<TransferOutcome<PartialDownload>, PartialDownloadError> {
    if !body.is_unconsumed() {
        return Err(PartialDownloadError::InvalidBodyState);
    }

    if cancellation.is_requested() {
        return Ok(TransferOutcome::Cancelled { written_bytes: 0 });
    }

    if let Some(store) = resume_store.as_ref()
        && store
            .find_state(id)
            .await
            .map_err(|error| PartialDownloadError::ResumeStore(error.kind()))?
            .is_some()
    {
        return Err(PartialDownloadError::ResumeStore(
            RepositoryErrorKind::ConstraintViolation,
        ));
    }

    if cancellation.is_requested() {
        return Ok(TransferOutcome::Cancelled { written_bytes: 0 });
    }

    let metadata = body.metadata().clone();
    let path = partial_path(directory, id);

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
        .map_err(|error| file_error(FileOperation::Create, error))?;

    let mut resume_state = None;

    if let (Some(store), Some(tag), Some(total)) = (
        resume_store.as_ref(),
        metadata.strong_etag(),
        metadata.content_length(),
    ) {
        let expected = HttpResumeState::new(tag.clone(), total, 0)
            .map_err(|_| PartialDownloadError::ResumeStore(RepositoryErrorKind::InvalidData))?;

        let persisted = store
            .initialize_state(id, tag.clone(), total)
            .await
            .map_err(|error| PartialDownloadError::ResumeStore(error.kind()))?;

        if persisted != expected {
            return Err(PartialDownloadError::ResumeStore(
                RepositoryErrorKind::InvalidData,
            ));
        }

        resume_state = Some(persisted);
    }

    stream_to_partial(StreamContext {
        body: TransferBody::Full(body),
        file,
        path,
        metadata,
        id,
        progress,
        cancellation,
        resume_store,
        resume_state,
        written_bytes: 0,
    })
    .await
}

pub(crate) enum ResumeBody {
    Full(HttpBody),
    Range(HttpRangeBody),
    Complete,
}

pub(crate) struct ResumeTransfer {
    pub(crate) body: ResumeBody,
    pub(crate) metadata: HttpInspection,
    pub(crate) state: HttpResumeState,
}

pub(crate) async fn resume_body_to_partial(
    transfer: ResumeTransfer,
    id: DownloadId,
    directory: &Path,
    progress: TransferProgress,
    cancellation: DownloadCancellation,
    resume_store: Arc<dyn ResumePersistence>,
) -> Result<TransferOutcome<PartialDownload>, PartialDownloadError> {
    let ResumeTransfer {
        body,
        metadata,
        state: resume_state,
    } = transfer;

    let body = match body {
        ResumeBody::Full(body) => TransferBody::Full(body),
        ResumeBody::Range(body) => TransferBody::Range(body),
        ResumeBody::Complete => TransferBody::Complete,
    };

    if !body.is_unconsumed() {
        return Err(PartialDownloadError::InvalidBodyState);
    }

    let written_bytes = resume_state.durable_bytes();

    if cancellation.is_requested() {
        return Ok(TransferOutcome::Cancelled { written_bytes });
    }

    let path = partial_path(directory, id);
    let file = OpenOptions::new()
        .write(true)
        .append(true)
        .open(&path)
        .await
        .map_err(|error| file_error(FileOperation::Open, error))?;

    let file_metadata = file
        .metadata()
        .await
        .map_err(|error| file_error(FileOperation::Open, error))?;

    if !file_metadata.is_file() || file_metadata.len() < written_bytes {
        return Err(PartialDownloadError::InvalidPartial);
    }

    if file_metadata.len() != written_bytes {
        file.set_len(written_bytes)
            .await
            .map_err(|error| file_error(FileOperation::Truncate, error))?;

        file.sync_all()
            .await
            .map_err(|error| file_error(FileOperation::Sync, error))?;
    }

    stream_to_partial(StreamContext {
        body,
        file,
        path,
        metadata,
        id,
        progress,
        cancellation,
        resume_store: Some(resume_store),
        resume_state: Some(resume_state),
        written_bytes,
    })
    .await
}

enum TransferBody {
    Full(HttpBody),
    Range(HttpRangeBody),
    Complete,
}

impl TransferBody {
    fn is_unconsumed(&self) -> bool {
        match self {
            Self::Full(body) => body.is_unconsumed(),
            Self::Range(body) => body.is_unconsumed(),
            Self::Complete => true,
        }
    }

    async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, HttpError> {
        match self {
            Self::Full(body) => body.next_chunk().await,
            Self::Range(body) => body.next_chunk().await,
            Self::Complete => Ok(None),
        }
    }
}

struct StreamContext {
    body: TransferBody,
    file: File,
    path: PathBuf,
    metadata: HttpInspection,
    id: DownloadId,
    progress: TransferProgress,
    cancellation: DownloadCancellation,
    resume_store: Option<Arc<dyn ResumePersistence>>,
    resume_state: Option<HttpResumeState>,
    written_bytes: u64,
}

async fn stream_to_partial(
    context: StreamContext,
) -> Result<TransferOutcome<PartialDownload>, PartialDownloadError> {
    let StreamContext {
        mut body,
        mut file,
        path,
        metadata,
        id,
        progress,
        cancellation,
        resume_store,
        mut resume_state,
        mut written_bytes,
    } = context;

    let mut last_checkpoint_bytes = written_bytes;
    let mut last_checkpoint_at = Instant::now();

    loop {
        let received = tokio::select! {
            biased;

            _ = cancellation.cancelled() => None,
            result = body.next_chunk() => Some(result),
        };

        let chunk = match received {
            None => {
                sync_checkpoint(
                    &mut file,
                    id,
                    resume_store.as_deref(),
                    &mut resume_state,
                    written_bytes,
                )
                .await?;

                return Ok(TransferOutcome::Cancelled { written_bytes });
            }

            Some(Err(error)) => {
                sync_checkpoint(
                    &mut file,
                    id,
                    resume_store.as_deref(),
                    &mut resume_state,
                    written_bytes,
                )
                .await?;

                return Err(error.into());
            }

            Some(Ok(None)) => break,
            Some(Ok(Some(chunk))) => chunk,
        };

        let size =
            u64::try_from(chunk.len()).map_err(|_| PartialDownloadError::ByteCountOverflow)?;

        let next_written_bytes = written_bytes
            .checked_add(size)
            .ok_or(PartialDownloadError::ByteCountOverflow)?;

        // Finish an accepted write before acknowledging cancellation.
        file.write_all(&chunk)
            .await
            .map_err(|error| file_error(FileOperation::Write, error))?;

        file.flush()
            .await
            .map_err(|error| file_error(FileOperation::Flush, error))?;

        written_bytes = next_written_bytes;
        progress.report_written(written_bytes);

        let checkpoint_due = resume_state.is_some()
            && (written_bytes - last_checkpoint_bytes >= 8 * 1024 * 1024
                || last_checkpoint_at.elapsed() >= Duration::from_secs(1));

        if checkpoint_due {
            sync_checkpoint(
                &mut file,
                id,
                resume_store.as_deref(),
                &mut resume_state,
                written_bytes,
            )
            .await?;

            last_checkpoint_bytes = written_bytes;
            last_checkpoint_at = Instant::now();
        }
    }

    sync_checkpoint(
        &mut file,
        id,
        resume_store.as_deref(),
        &mut resume_state,
        written_bytes,
    )
    .await?;

    drop(file);

    if cancellation.is_requested() {
        return Ok(TransferOutcome::Cancelled { written_bytes });
    }

    Ok(TransferOutcome::Finished(PartialDownload {
        id,
        path,
        written_bytes,
        metadata,
    }))
}

async fn sync_checkpoint(
    file: &mut File,
    id: DownloadId,
    store: Option<&dyn ResumePersistence>,
    state: &mut Option<HttpResumeState>,
    written_bytes: u64,
) -> Result<(), PartialDownloadError> {
    file.flush()
        .await
        .map_err(|error| file_error(FileOperation::Flush, error))?;

    file.sync_all()
        .await
        .map_err(|error| file_error(FileOperation::Sync, error))?;

    let Some(current) = state.as_ref() else {
        return Ok(());
    };

    if written_bytes == current.durable_bytes() {
        return Ok(());
    }

    let store = store.ok_or(PartialDownloadError::ResumeStore(
        RepositoryErrorKind::InvalidData,
    ))?;

    let candidate = HttpResumeState::new(
        current.strong_etag().clone(),
        current.total_bytes(),
        written_bytes,
    )
    .map_err(|_| PartialDownloadError::ResumeStore(RepositoryErrorKind::InvalidData))?;

    let persisted = store
        .save_state(id, candidate.clone())
        .await
        .map_err(|error| PartialDownloadError::ResumeStore(error.kind()))?;

    if persisted != candidate {
        return Err(PartialDownloadError::ResumeStore(
            RepositoryErrorKind::InvalidData,
        ));
    }

    *state = Some(persisted);

    Ok(())
}

fn file_error(operation: FileOperation, error: io::Error) -> PartialDownloadError {
    PartialDownloadError::File {
        operation,
        kind: error.kind(),
    }
}
