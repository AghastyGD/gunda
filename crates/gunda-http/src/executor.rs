use std::io;

use gunda_core::application::{
    DownloadExecutor, ExecutionInput, ExecutionOutput, PreparedTransfer, StagedTransfer,
    TransferProgress,
};
use gunda_core::download::{
    DownloadDestination, DownloadFailure, DownloadId, FailureKind, ResolvedDestination,
    ResourceDescriptor, ResourceKind,
};

use crate::destination::plan_destination;
use crate::partial::write_body_to_partial;
use crate::{
    FinalizeError, HttpBody, HttpClient, HttpError, PartialDownload, PartialDownloadError,
    finalize_download,
};

/// Executes a direct HTTP resource as a file.
pub struct HttpExecutor {
    client: HttpClient,
}

impl HttpExecutor {
    #[must_use]
    pub fn new(client: HttpClient) -> Self {
        Self { client }
    }
}

/// An opened GET response that has not been written to staging.
pub struct HttpPreparedTransfer {
    body: HttpBody,
    id: DownloadId,
    destination: DownloadDestination,
    planned_destination: ResolvedDestination,
}

/// A complete partial file awaiting publication.
pub struct HttpStagedTransfer {
    partial: PartialDownload,
    destination: DownloadDestination,
}

impl DownloadExecutor for HttpExecutor {
    type Prepared = HttpPreparedTransfer;

    async fn prepare(&self, input: ExecutionInput) -> Result<Self::Prepared, DownloadFailure> {
        let ExecutionInput {
            id,
            request,
            destination,
        } = input;

        let (destination, planned_destination) =
            plan_destination(id, request.url(), &destination).map_err(finalization_failure)?;

        let body = self.client.open(&request).await.map_err(http_failure)?;

        Ok(HttpPreparedTransfer {
            body,
            id,
            destination,
            planned_destination,
        })
    }
}

impl PreparedTransfer for HttpPreparedTransfer {
    type Staged = HttpStagedTransfer;

    fn resource(&self) -> ResourceDescriptor {
        ResourceDescriptor::new(
            ResourceKind::File,
            None,
            self.body.metadata().content_type().map(str::to_owned),
        )
    }

    fn total_bytes(&self) -> Option<u64> {
        self.body.metadata().content_length()
    }

    fn planned_destination(&self) -> ResolvedDestination {
        self.planned_destination.clone()
    }

    async fn transfer_with_progress(
        self,
        progress: TransferProgress,
    ) -> Result<Self::Staged, DownloadFailure> {
        let Self {
            body,
            id,
            destination,
            ..
        } = self;

        let partial = write_body_to_partial(body, id, destination.directory(), progress)
            .await
            .map_err(partial_failure)?;

        Ok(HttpStagedTransfer {
            partial,
            destination,
        })
    }
}

impl StagedTransfer for HttpStagedTransfer {
    fn written_bytes(&self) -> u64 {
        self.partial.written_bytes()
    }

    async fn finalize(self) -> Result<ExecutionOutput, DownloadFailure> {
        let Self {
            partial,
            destination,
        } = self;

        let finalized = finalize_download(partial, &destination)
            .await
            .map_err(|failure| finalization_failure(failure.error()))?;

        Ok(ExecutionOutput {
            destination: finalized.destination().clone(),
            written_bytes: finalized.written_bytes(),
            cleanup_pending: finalized.partial_cleanup_error().is_some(),
        })
    }
}

fn http_failure(error: HttpError) -> DownloadFailure {
    let (kind, message, retryable) = match error {
        HttpError::Configuration => (
            FailureKind::Internal,
            "could not configure the HTTP client",
            false,
        ),
        HttpError::InvalidRequest => (
            FailureKind::UnsupportedResource,
            "request context is invalid or unsupported",
            false,
        ),
        HttpError::Timeout => (FailureKind::Network, "HTTP request timed out", true),
        HttpError::Transport => (FailureKind::Network, "HTTP transport failed", true),
        HttpError::UnexpectedStatus(401 | 403) => (
            FailureKind::Authentication,
            "remote server rejected request authorization",
            false,
        ),
        HttpError::UnexpectedStatus(408 | 429 | 500..=599) => (
            FailureKind::RemoteRejected,
            "remote server returned a potentially temporary failure",
            true,
        ),
        HttpError::UnexpectedStatus(_) => (
            FailureKind::RemoteRejected,
            "remote server returned an unsupported response status",
            false,
        ),
        HttpError::InvalidMetadata => (
            FailureKind::InvalidResponse,
            "HTTP response metadata is invalid",
            false,
        ),
        HttpError::UnsupportedEncoding => (
            FailureKind::UnsupportedResource,
            "HTTP content encoding is unsupported",
            false,
        ),
        HttpError::BodyLengthMismatch => (
            FailureKind::Integrity,
            "HTTP body length does not match response metadata",
            false,
        ),
        HttpError::ByteCountOverflow => (
            FailureKind::UnsupportedResource,
            "resource size exceeds the supported range",
            false,
        ),
    };

    DownloadFailure::new(kind, message, retryable)
}

fn partial_failure(error: PartialDownloadError) -> DownloadFailure {
    match error {
        PartialDownloadError::Http(error) => http_failure(error),

        PartialDownloadError::File { kind, .. } => filesystem_failure(kind),

        PartialDownloadError::InvalidBodyState => DownloadFailure::new(
            FailureKind::Internal,
            "transfer received an unavailable or previously consumed HTTP body",
            false,
        ),

        PartialDownloadError::ByteCountOverflow => DownloadFailure::new(
            FailureKind::UnsupportedResource,
            "resource size exceeds the supported range",
            false,
        ),
    }
}

fn finalization_failure(error: FinalizeError) -> DownloadFailure {
    match error {
        FinalizeError::File(kind) => filesystem_failure(kind),

        FinalizeError::InvalidPartial => DownloadFailure::new(
            FailureKind::Integrity,
            "partial file no longer matches the transfer result",
            false,
        ),

        FinalizeError::InvalidFilename => DownloadFailure::new(
            FailureKind::Storage,
            "destination filename is invalid or reserved",
            false,
        ),

        FinalizeError::DifferentDirectory => DownloadFailure::new(
            FailureKind::Storage,
            "staging and final output must use the same directory",
            false,
        ),

        FinalizeError::NameAttemptsExhausted => DownloadFailure::new(
            FailureKind::Storage,
            "no destination name was available within the attempt limit",
            false,
        ),
    }
}

fn filesystem_failure(kind: io::ErrorKind) -> DownloadFailure {
    let (failure_kind, message) = match kind {
        io::ErrorKind::PermissionDenied => (
            FailureKind::PermissionDenied,
            "filesystem permission was denied",
        ),
        io::ErrorKind::StorageFull => (
            FailureKind::DiskFull,
            "filesystem has insufficient free space",
        ),
        io::ErrorKind::AlreadyExists => (
            FailureKind::Storage,
            "an output or staging path already exists",
        ),
        io::ErrorKind::NotFound => (
            FailureKind::Storage,
            "a required filesystem path does not exist",
        ),
        _ => (FailureKind::Storage, "filesystem operation failed"),
    };

    DownloadFailure::new(failure_kind, message, false)
}
