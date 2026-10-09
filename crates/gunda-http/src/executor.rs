use std::io;
use std::sync::Arc;

use gunda_core::application::{
    DownloadCancellation, DownloadExecutor, ExecutionInput, ExecutionOutput, PreparedTransfer,
    StagedTransfer, TransferOutcome, TransferProgress,
};
use gunda_core::download::{
    DownloadDestination, DownloadFailure, DownloadId, FailureKind, ResolvedDestination,
    ResourceDescriptor, ResourceKind,
};

use crate::HttpResumeStore;
use crate::destination::plan_destination;
use crate::partial::{
    ResumeBody, ResumeTransfer, resume_body_to_partial, write_body_to_partial_with_resume,
};
use crate::recovery::{ResumeFileError, inspect_resume_candidate};
use crate::resume_state::ResumePersistence;
use crate::{
    FinalizeError, HttpBody, HttpClient, HttpError, HttpInspection, HttpResumeState,
    PartialDownload, PartialDownloadError, finalize_download, partial_path,
};

/// Executes a direct HTTP resource as a file.
pub struct HttpExecutor {
    client: HttpClient,
    resume_store: Option<Arc<dyn ResumePersistence>>,
}

impl HttpExecutor {
    #[must_use]
    pub fn new(client: HttpClient) -> Self {
        Self {
            client,
            resume_store: None,
        }
    }

    #[must_use]
    pub fn with_resume_store<S>(client: HttpClient, store: S) -> Self
    where
        S: HttpResumeStore + 'static,
    {
        Self {
            client,
            resume_store: Some(Arc::new(store)),
        }
    }
}

/// An opened GET response that has not been written to staging.
pub struct HttpPreparedTransfer {
    body: HttpPreparedBody,
    metadata: HttpInspection,
    resource: ResourceDescriptor,
    starting_bytes: u64,
    id: DownloadId,
    destination: DownloadDestination,
    planned_destination: ResolvedDestination,
    resume_store: Option<Arc<dyn ResumePersistence>>,
}

enum HttpPreparedBody {
    Fresh(HttpBody),
    Resumed {
        body: ResumeBody,
        state: HttpResumeState,
    },
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
            progress,
            resource,
            resolved_destination,
        } = input;

        let (destination, default_destination) =
            plan_destination(id, request.url(), &destination).map_err(finalization_failure)?;

        let Some(store) = self.resume_store.as_ref() else {
            let body = self.client.open(&request).await.map_err(http_failure)?;
            let metadata = body.metadata().clone();
            let resource = http_resource(resource.as_ref(), &metadata)?;

            return Ok(HttpPreparedTransfer {
                body: HttpPreparedBody::Fresh(body),
                metadata,
                resource,
                starting_bytes: 0,
                id,
                destination,
                planned_destination: default_destination,
                resume_store: None,
            });
        };

        let resume_state = store
            .find_state(id)
            .await
            .map_err(|_| storage_failure("could not load HTTP resume state"))?;

        let Some(resume_state) = resume_state else {
            let body = self.client.open(&request).await.map_err(http_failure)?;
            let metadata = body.metadata().clone();
            let resource = http_resource(resource.as_ref(), &metadata)?;

            return Ok(HttpPreparedTransfer {
                body: HttpPreparedBody::Fresh(body),
                metadata,
                resource,
                starting_bytes: 0,
                id,
                destination,
                planned_destination: default_destination,
                resume_store: self.resume_store.clone(),
            });
        };

        if progress.total_bytes() != Some(resume_state.total_bytes()) {
            return Err(recovery_integrity(
                "saved download size does not match HTTP resume state",
            ));
        }

        if resource
            .as_ref()
            .is_some_and(|resource| resource.kind() != ResourceKind::File)
        {
            return Err(recovery_integrity(
                "saved resource type is not resumable as a direct HTTP file",
            ));
        }

        let planned_destination = resolved_destination.ok_or_else(|| {
            recovery_integrity("interrupted HTTP download has no planned destination")
        })?;

        inspect_resume_candidate(
            &partial_path(destination.directory(), id),
            (resume_state.durable_bytes() == resume_state.total_bytes())
                .then(|| planned_destination.final_path()),
            resume_state.durable_bytes(),
        )
        .await
        .map_err(resume_file_failure)?;

        let (body, metadata) = if resume_state.durable_bytes() == resume_state.total_bytes() {
            (
                ResumeBody::Complete,
                HttpInspection::resumed(
                    resume_state.total_bytes(),
                    resource
                        .as_ref()
                        .and_then(ResourceDescriptor::content_type)
                        .map(str::to_owned),
                    resume_state.strong_etag().clone(),
                ),
            )
        } else if resume_state.durable_bytes() == 0 {
            let body = self.client.open(&request).await.map_err(http_failure)?;

            if body.metadata().content_length() != Some(resume_state.total_bytes())
                || body.metadata().strong_etag() != Some(resume_state.strong_etag())
            {
                return Err(recovery_integrity(
                    "remote resource no longer matches HTTP resume state",
                ));
            }

            let metadata = body.metadata().clone();
            (ResumeBody::Full(body), metadata)
        } else {
            let range = self
                .client
                .open_range(
                    &request,
                    resume_state.durable_bytes(),
                    resume_state.total_bytes(),
                    resume_state.strong_etag(),
                )
                .await
                .map_err(resume_http_failure)?;

            let metadata = HttpInspection::resumed(
                resume_state.total_bytes(),
                range
                    .metadata()
                    .content_type()
                    .or_else(|| resource.as_ref().and_then(ResourceDescriptor::content_type))
                    .map(str::to_owned),
                resume_state.strong_etag().clone(),
            );

            (ResumeBody::Range(range), metadata)
        };

        let resource = http_resource(resource.as_ref(), &metadata)?;

        Ok(HttpPreparedTransfer {
            body: HttpPreparedBody::Resumed {
                body,
                state: resume_state.clone(),
            },
            metadata,
            resource,
            starting_bytes: resume_state.durable_bytes(),
            id,
            destination,
            planned_destination,
            resume_store: self.resume_store.clone(),
        })
    }
}

impl PreparedTransfer for HttpPreparedTransfer {
    type Staged = HttpStagedTransfer;

    fn resource(&self) -> ResourceDescriptor {
        self.resource.clone()
    }

    fn total_bytes(&self) -> Option<u64> {
        self.metadata.content_length()
    }

    fn starting_bytes(&self) -> u64 {
        self.starting_bytes
    }

    fn planned_destination(&self) -> ResolvedDestination {
        self.planned_destination.clone()
    }

    async fn transfer_controlled(
        self,
        progress: TransferProgress,
        cancellation: DownloadCancellation,
    ) -> Result<TransferOutcome<Self::Staged>, DownloadFailure> {
        let Self {
            body,
            metadata,
            id,
            destination,
            resume_store,
            ..
        } = self;

        let outcome = match body {
            HttpPreparedBody::Fresh(body) => {
                write_body_to_partial_with_resume(
                    body,
                    id,
                    destination.directory(),
                    progress,
                    cancellation,
                    resume_store,
                )
                .await
            }
            HttpPreparedBody::Resumed { body, state } => {
                let store = resume_store.ok_or_else(|| {
                    storage_failure("HTTP resume state has no persistence adapter")
                })?;

                resume_body_to_partial(
                    ResumeTransfer {
                        body,
                        metadata,
                        state,
                    },
                    id,
                    destination.directory(),
                    progress,
                    cancellation,
                    store,
                )
                .await
            }
        }
        .map_err(partial_failure)?;

        Ok(match outcome {
            TransferOutcome::Finished(partial) => TransferOutcome::Finished(HttpStagedTransfer {
                partial,
                destination,
            }),
            TransferOutcome::Cancelled { written_bytes } => {
                TransferOutcome::Cancelled { written_bytes }
            }
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

fn resume_http_failure(error: HttpError) -> DownloadFailure {
    match error {
        HttpError::UnexpectedStatus(200 | 412 | 416) => {
            recovery_integrity("remote resource could not satisfy the saved HTTP checkpoint")
        }
        error => http_failure(error),
    }
}

fn http_resource(
    previous: Option<&ResourceDescriptor>,
    metadata: &HttpInspection,
) -> Result<ResourceDescriptor, DownloadFailure> {
    if previous.is_some_and(|resource| resource.kind() != ResourceKind::File) {
        return Err(recovery_integrity(
            "saved resource type is not a direct HTTP file",
        ));
    }

    Ok(ResourceDescriptor::new(
        ResourceKind::File,
        previous
            .and_then(ResourceDescriptor::display_name)
            .map(str::to_owned),
        metadata.content_type().map(str::to_owned).or_else(|| {
            previous
                .and_then(ResourceDescriptor::content_type)
                .map(str::to_owned)
        }),
    ))
}

fn resume_file_failure(error: ResumeFileError) -> DownloadFailure {
    match error {
        ResumeFileError::PartialUnavailable(kind) | ResumeFileError::OutputUnavailable(kind) => {
            filesystem_failure(kind)
        }
        ResumeFileError::MissingPartial => recovery_integrity("saved HTTP partial file is missing"),
        ResumeFileError::PartialTooShort => {
            recovery_integrity("saved HTTP partial file is shorter than its durable checkpoint")
        }
        ResumeFileError::UnsafePartial => {
            recovery_integrity("saved HTTP partial path is not a regular file")
        }
        ResumeFileError::OutputExists => DownloadFailure::new(
            FailureKind::Storage,
            "planned output already exists during HTTP recovery",
            false,
        ),
    }
}

fn recovery_integrity(message: &'static str) -> DownloadFailure {
    DownloadFailure::new(FailureKind::Integrity, message, false)
}

fn storage_failure(message: &'static str) -> DownloadFailure {
    DownloadFailure::new(FailureKind::Storage, message, false)
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

        PartialDownloadError::InvalidPartial => {
            recovery_integrity("partial file no longer matches its durable HTTP checkpoint")
        }

        PartialDownloadError::ResumeStore(_) => DownloadFailure::new(
            FailureKind::Storage,
            "could not persist HTTP resume state",
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
