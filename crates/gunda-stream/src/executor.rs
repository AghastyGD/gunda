//! Executing a segmented-media plan through the shared download contract.
//!
//! The engine fetches the planned resources in order and appends them to the
//! same staging file a plain HTTP download uses, then publishes it through the
//! same validated `finalize_download` path. Nothing about the job lifecycle,
//! progress reporting, or conflict handling is reimplemented here.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gunda_core::application::{
    DownloadCancellation, DownloadExecutor, ExecutionInput, ExecutionOutput, PreparedTransfer,
    StagedTransfer, TransferOutcome, TransferProgress,
};
use gunda_core::download::{
    DownloadDestination, DownloadFailure, DownloadId, FailureKind, RequestContext, RequestHeader,
    ResolvedDestination, ResourceDescriptor,
};
use gunda_http::{
    FinalizeError, HttpBody, HttpClient, HttpError, HttpRangeBody, PartialDownload,
    PartialDownloadError, finalize_download, partial_path, plan_destination, url_filename,
};
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::container::{Container, SNIFF_BYTES, detect};
use crate::dash;
use crate::error::ManifestError;
use crate::hls;
use crate::mux::{MuxError, mux_mp4};
use crate::plan::{HlsOutcome, TrackPlan, plan_dash, plan_hls};
use crate::presentation::{self, Presentation};

/// Largest manifest accepted from the network.
///
/// Manifests are text and small; anything larger is not one, and reading it
/// would be wasted work.
const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

/// Most segments whose absent lengths are probed before the total is left
/// unknown instead.
///
/// Probing costs one metadata request per segment. Past this many the cost is
/// no longer worth exactness, and reporting nothing is more honest than
/// reporting an approximation as though it were measured.
const MAX_PROBED_RESOURCES: usize = 512;

/// How many metadata probes are in flight at once.
const PROBE_CONCURRENCY: usize = 8;

/// Extension used when the media's container cannot be recognised.
const UNKNOWN_EXTENSION: &str = "bin";

/// Executes an HLS or DASH presentation as a single output file.
pub struct StreamExecutor {
    client: Arc<HttpClient>,
}

impl StreamExecutor {
    #[must_use]
    pub fn new(client: HttpClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    /// Describes the tracks a presentation offers without downloading any of it.
    ///
    /// A capture reports one URL, and a presentation reached that way usually
    /// carries several qualities and tracks. This reads the manifest so the
    /// caller can let the user choose before anything is fetched.
    pub async fn inspect(&self, request: &RequestContext) -> Result<Presentation, DownloadFailure> {
        presentation::inspect(&self.client, request).await
    }
}

/// A resolved plan whose segments have not been fetched.
pub struct StreamPreparedTransfer {
    client: Arc<HttpClient>,
    id: DownloadId,
    plan: TrackPlan,
    manifest_url: Url,
    headers: Vec<RequestHeader>,
    content_type: Option<String>,
    total_bytes: Option<u64>,
    destination: DownloadDestination,
    planned_destination: ResolvedDestination,
}

/// A completely staged presentation awaiting publication.
pub struct StreamStagedTransfer {
    id: DownloadId,
    destination: DownloadDestination,
    written_bytes: u64,
    content_type: Option<String>,
}

/// One planned resource's response body.
///
/// A whole resource and one byte interval of a larger resource stream
/// identically but come from different client calls, so they are unified here
/// rather than duplicating the append loop.
enum SegmentBody {
    Whole(HttpBody),
    Range(HttpRangeBody),
}

impl SegmentBody {
    /// Appends the next chunk of the body to `file` and returns its length.
    async fn write_next(
        &mut self,
        file: &mut tokio::fs::File,
    ) -> Result<Option<u64>, DownloadFailure> {
        let chunk = match self {
            Self::Whole(body) => body.next_chunk().await.map_err(http_failure)?,
            Self::Range(body) => body.next_chunk().await.map_err(http_failure)?,
        };

        let Some(chunk) = chunk else {
            return Ok(None);
        };

        file.write_all(&chunk).await.map_err(staging_failure)?;

        Ok(Some(
            u64::try_from(chunk.len()).map_err(|_| size_overflow())?,
        ))
    }
}

impl DownloadExecutor for StreamExecutor {
    type Prepared = StreamPreparedTransfer;

    async fn prepare(&self, input: ExecutionInput) -> Result<Self::Prepared, DownloadFailure> {
        let ExecutionInput {
            id,
            request,
            destination,
            ..
        } = input;

        let (plan, content_type) = self.plan(&request).await?;

        // What the media is and how large it will be are both judgements about
        // the bytes, not about the manifest, and neither depends on the other.
        let (container, total_bytes) = tokio::join!(
            self.container(&request, &plan),
            self.measure(&request, &plan)
        );

        // A presentation whose tracks are fetched apart is published as the
        // one file they are muxed into, not as whichever track happened to be
        // inspected: reading the first resource says what that track is, and
        // that track is not the output.
        let container = if plan.track_count() > 1 {
            Some(Container::Mp4)
        } else {
            container
        };

        let named = name_output(destination, request.url(), container);
        let (destination, planned_destination) =
            plan_destination(id, request.url(), &named).map_err(finalization_failure)?;

        Ok(StreamPreparedTransfer {
            client: Arc::clone(&self.client),
            id,
            plan,
            manifest_url: request.url().clone(),
            headers: request.headers().to_vec(),
            content_type,
            total_bytes,
            destination,
            planned_destination,
        })
    }
}

impl StreamExecutor {
    /// Fetches and plans a manifest, following one master-playlist indirection.
    async fn plan(
        &self,
        request: &RequestContext,
    ) -> Result<(TrackPlan, Option<String>), DownloadFailure> {
        let (bytes, content_type) = open_manifest(&self.client, request).await?;
        let base = request.url().clone();
        let text = std::str::from_utf8(&bytes).map_err(|_| not_textual())?;

        if !text.trim_start().starts_with("#EXTM3U") {
            let mpd = dash::parse(text).map_err(manifest_failure)?;

            return Ok((
                plan_dash(&base, &mpd).map_err(manifest_failure)?,
                content_type,
            ));
        }

        let playlist = hls::parse(text).map_err(manifest_failure)?;

        match plan_hls(&base, &playlist).map_err(manifest_failure)? {
            // The entry point was already a media playlist.
            HlsOutcome::Ready(plan) => Ok((plan, content_type)),
            HlsOutcome::FetchVariant { url, audio, .. } => {
                let video = self.fetch_media_playlist(&url, &base, request, 0).await?;

                // A variant that names audio renditions carries no audio of its
                // own, so the rendition the master points at is part of the
                // download: fetched apart, muxed together.
                let Some(audio) = audio else {
                    return Ok((video, content_type));
                };

                let track = self.fetch_media_playlist(&audio, &base, request, 1).await?;

                Ok((
                    video.with_audio(track).map_err(manifest_failure)?,
                    content_type,
                ))
            }
        }
    }

    /// Fetches one media playlist and plans it as a track.
    async fn fetch_media_playlist(
        &self,
        url: &Url,
        base: &Url,
        request: &RequestContext,
        track: u8,
    ) -> Result<TrackPlan, DownloadFailure> {
        let playlist_request = segment_request(url, base, request.headers());
        let (bytes, _) = open_manifest(&self.client, &playlist_request).await?;
        let text = std::str::from_utf8(&bytes).map_err(|_| not_textual())?;
        let playlist = hls::parse(text).map_err(manifest_failure)?;

        match plan_hls(url, &playlist).map_err(manifest_failure)? {
            HlsOutcome::Ready(plan) => Ok(plan.retracked(track)),
            HlsOutcome::FetchVariant { .. } => Err(DownloadFailure::new(
                FailureKind::UnsupportedResource,
                "nested master playlists are not supported",
                false,
            )),
        }
    }

    /// Reads the container the finished file will be.
    ///
    /// Best effort: a stream that cannot be read now is not a reason to fail a
    /// download that may well succeed, so an unreadable resource falls back to
    /// what the manifest format itself implies.
    async fn container(&self, request: &RequestContext, plan: &TrackPlan) -> Option<Container> {
        let head = self.read_head(request, plan).await?;

        detect(&head, plan.container_hint())
    }

    /// Reads the leading bytes of the first planned resource.
    async fn read_head(&self, request: &RequestContext, plan: &TrackPlan) -> Option<Vec<u8>> {
        let first = plan.resources().first()?;
        let segment = segment_request(first.url(), request.url(), request.headers());

        let mut head = match first.byte_interval() {
            // A resource addressed part way into a larger one must be read where
            // the manifest put it, or the bytes belong to something else.
            Some((start, _)) if start > 0 => {
                let limit = start.saturating_add(SNIFF_BYTES as u64 - 1);
                let mut body = self
                    .client
                    .open_byte_range(&segment, start, limit)
                    .await
                    .ok()?;
                let mut head = Vec::new();

                while let Some(chunk) = body.next_chunk().await.ok()? {
                    head.extend_from_slice(&chunk);
                }

                head
            }
            // Otherwise one chunk of the resource is enough, and the body is
            // dropped once it has arrived: only the header was wanted.
            _ => {
                let mut body = self.client.open(&segment).await.ok()?;
                let mut head = Vec::new();

                while let Some(chunk) = body.next_chunk().await.ok()? {
                    head.extend_from_slice(&chunk);

                    if head.len() >= SNIFF_BYTES {
                        break;
                    }
                }

                head
            }
        };

        head.truncate(SNIFF_BYTES);

        Some(head)
    }

    /// The exact size of the finished media, when it can be known up front.
    ///
    /// A manifest frequently declares no lengths at all: an `EXTINF` playlist
    /// says how long each segment plays, not how large it is. The size is then
    /// read from the origin with one metadata request per segment, so the
    /// interface can show a measured total rather than an estimate.
    async fn measure(&self, request: &RequestContext, plan: &TrackPlan) -> Option<u64> {
        if let Some(total) = plan.known_total_bytes() {
            return Some(total);
        }

        if plan.resource_count() > MAX_PROBED_RESOURCES {
            return None;
        }

        let mut probes = tokio::task::JoinSet::new();
        let mut total = 0_u64;

        for resource in plan.resources() {
            let segment = segment_request(resource.url(), request.url(), request.headers());
            let client = Arc::clone(&self.client);

            probes.spawn(async move {
                client
                    .inspect(&segment)
                    .await
                    .map(|metadata| metadata.content_length())
                    .unwrap_or_default()
            });

            if probes.len() >= PROBE_CONCURRENCY {
                total = total.checked_add(finished_probe(&mut probes).await?)?;
            }
        }

        while !probes.is_empty() {
            total = total.checked_add(finished_probe(&mut probes).await?)?;
        }

        Some(total)
    }
}

/// Collects one finished metadata probe.
///
/// A probe that panicked, was cancelled, or reported no length leaves the total
/// unknowable, which `None` carries to the caller.
async fn finished_probe(probes: &mut tokio::task::JoinSet<Option<u64>>) -> Option<u64> {
    probes.join_next().await?.ok()?
}

/// Names the output after the media it will contain.
///
/// A manifest URL names a playlist, so publishing its last path segment
/// unchanged would save `index.m3u8` for a file that is really an MP4 or a
/// transport stream. A filename the caller chose always wins; otherwise the
/// name the shared namer derived is kept and only its extension is replaced
/// with the container the bytes actually are.
fn name_output(
    destination: DownloadDestination,
    manifest_url: &Url,
    container: Option<Container>,
) -> DownloadDestination {
    if destination.preferred_filename().is_some() {
        return destination;
    }

    let Some(name) = output_filename(manifest_url, container) else {
        return destination;
    };

    DownloadDestination::new(
        destination.directory().to_path_buf(),
        Some(name),
        destination.conflict_policy(),
    )
}

/// The filename a presentation is published under.
///
/// A manifest URL names a playlist, so its last path segment addresses the
/// wrong thing: `index.m3u8` for a file that is really an MP4 or a transport
/// stream. The validated stem of the URL is kept and only its extension is
/// replaced with the container the media actually is, so the result is as safe
/// as the name it came from and does not need validating again.
///
/// A presentation that is downloaded from a track's own playlist is still named
/// after the presentation, not after the track, because variant playlists are
/// frequently all called `index.m3u8`.
#[must_use]
pub fn output_filename(manifest_url: &Url, container: Option<Container>) -> Option<String> {
    let name = url_filename(manifest_url)?;

    Some(replace_extension(&name, container))
}

/// Replaces a filename's extension with a container's.
fn replace_extension(name: &str, container: Option<Container>) -> String {
    Path::new(name)
        .with_extension(container.map_or(UNKNOWN_EXTENSION, Container::extension))
        .to_string_lossy()
        .into_owned()
}

impl PreparedTransfer for StreamPreparedTransfer {
    type Staged = StreamStagedTransfer;

    fn resource(&self) -> ResourceDescriptor {
        ResourceDescriptor::new(
            self.plan.kind().resource_kind(),
            self.plan.label().map(str::to_owned),
            self.content_type.clone(),
        )
    }

    fn total_bytes(&self) -> Option<u64> {
        self.total_bytes
    }

    /// Segmented downloads cannot stop at a durable checkpoint yet: the engine
    /// has no record of which planned segments are already in the staging file.
    fn supports_pause(&self) -> bool {
        false
    }

    fn planned_destination(&self) -> ResolvedDestination {
        self.planned_destination.clone()
    }

    async fn transfer_controlled(
        self,
        progress: TransferProgress,
        cancellation: DownloadCancellation,
    ) -> Result<TransferOutcome<Self::Staged>, DownloadFailure> {
        if self.plan.track_count() > 1 {
            return self.transfer_muxed(progress, cancellation).await;
        }

        let path = partial_path(self.destination.directory(), self.id);
        let (written, stopped) = self
            .write_track(0, &path, 0, &progress, &cancellation)
            .await?;

        if stopped {
            return Ok(TransferOutcome::Stopped {
                written_bytes: written,
            });
        }

        Ok(TransferOutcome::Finished(StreamStagedTransfer {
            id: self.id,
            destination: self.destination,
            written_bytes: written,
            content_type: self.content_type,
        }))
    }
}

impl StreamPreparedTransfer {
    /// Fetches every track and combines them into the one published file.
    ///
    /// Separate tracks cannot be concatenated: only muxing produces a file a
    /// player can use. Each track is therefore fetched into a staging file of
    /// its own and the two are then written into the partial file the shared
    /// machinery publishes from. The staging files do not outlive the transfer.
    async fn transfer_muxed(
        self,
        progress: TransferProgress,
        cancellation: DownloadCancellation,
    ) -> Result<TransferOutcome<StreamStagedTransfer>, DownloadFailure> {
        let directory = self.destination.directory().to_path_buf();
        let mut staging = StagingTracks::default();
        let mut written = 0_u64;
        let mut tracks = Vec::new();

        for track in 0..self.plan.track_count() as u8 {
            let path = staging.reserve(track_path(&directory, self.id, track));
            let (bytes, stopped) = self
                .write_track(track, &path, written, &progress, &cancellation)
                .await?;

            written = written.checked_add(bytes).ok_or_else(size_overflow)?;

            if stopped {
                return Ok(TransferOutcome::Stopped {
                    written_bytes: written,
                });
            }

            tracks.push(path);
        }

        let (Some(video), Some(audio)) = (tracks.first(), tracks.get(1)) else {
            return Err(internal_failure(
                "a muxed download holds exactly two tracks",
            ));
        };

        let combined = mux_mp4(video, audio, &partial_path(&directory, self.id))
            .await
            .map_err(mux_failure)?;

        Ok(TransferOutcome::Finished(StreamStagedTransfer {
            id: self.id,
            destination: self.destination,
            written_bytes: combined,
            content_type: self.content_type,
        }))
    }

    /// Fetches one track's resources into its own staging file.
    ///
    /// Returns how many bytes were appended, and whether the transfer was
    /// stopped; `base` is what the progress counts from, so a second track
    /// continues the count rather than restarting it.
    async fn write_track(
        &self,
        track: u8,
        path: &Path,
        base: u64,
        progress: &TransferProgress,
        cancellation: &DownloadCancellation,
    ) -> Result<(u64, bool), DownloadFailure> {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
            .map_err(staging_failure)?;

        let mut written = 0_u64;

        for resource in self.plan.resources_of(track) {
            if cancellation.is_requested() {
                return Ok((written, true));
            }

            let request = segment_request(resource.url(), &self.manifest_url, &self.headers);

            // A ranged segment is one interval of a larger resource, so it must
            // be requested as a range; the body is then only that interval.
            let mut body = match resource.byte_interval() {
                Some((first, last)) => SegmentBody::Range(
                    self.client
                        .open_byte_range(&request, first, last)
                        .await
                        .map_err(http_failure)?,
                ),
                None => SegmentBody::Whole(self.client.open(&request).await.map_err(http_failure)?),
            };

            while let Some(length) = body.write_next(&mut file).await? {
                written = written.checked_add(length).ok_or_else(size_overflow)?;
                progress.report_written(base.checked_add(written).ok_or_else(size_overflow)?);
            }
        }

        file.flush().await.map_err(staging_failure)?;
        file.sync_all().await.map_err(staging_failure)?;

        Ok((written, false))
    }
}

/// The track files of one muxed download.
///
/// They are staging files like the partial file the engine writes, and must not
/// outlive the transfer: whatever happens, they are removed when this is
/// dropped. The muxed file is a different one and is left alone.
#[derive(Default)]
struct StagingTracks {
    paths: Vec<PathBuf>,
}

impl StagingTracks {
    /// Remembers a track file so it cannot be left behind.
    fn reserve(&mut self, path: PathBuf) -> PathBuf {
        self.paths.push(path.clone());

        path
    }
}

impl Drop for StagingTracks {
    fn drop(&mut self) {
        for path in &self.paths {
            // Best effort: a leftover staging file wastes space, but failing a
            // download over one that could not be removed would be worse.
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The staging file one track of a muxed download is fetched into.
fn track_path(directory: &Path, id: DownloadId, track: u8) -> PathBuf {
    let partial = partial_path(directory, id);
    let mut name = partial.file_name().unwrap_or_default().to_owned();

    name.push(if track == 1 { ".audio" } else { ".video" });

    partial.with_file_name(name)
}

/// Turns a muxing failure into a download failure.
fn mux_failure(error: MuxError) -> DownloadFailure {
    match error {
        // The tracks were fetched from the origin, so what is wrong with them
        // says something about the origin's media.
        MuxError::Malformed(_) | MuxError::Unsupported(_) => DownloadFailure::new(
            FailureKind::InvalidResponse,
            "the separate tracks are not a form that can be combined into one file",
            false,
        ),
        MuxError::TooLarge => DownloadFailure::new(
            FailureKind::UnsupportedResource,
            "the separate tracks are too large to be combined into one file",
            false,
        ),
        MuxError::Io(_) => staging_failure(std::io::Error::other("muxing")),
    }
}

impl StagedTransfer for StreamStagedTransfer {
    fn written_bytes(&self) -> u64 {
        self.written_bytes
    }

    async fn finalize(self) -> Result<ExecutionOutput, DownloadFailure> {
        let partial = PartialDownload::adopt(
            self.id,
            self.destination.directory(),
            self.written_bytes,
            self.content_type,
        )
        .await
        .map_err(partial_failure)?;

        let finalized = finalize_download(partial, &self.destination)
            .await
            .map_err(|failure| finalization_failure(failure.error()))?;

        Ok(ExecutionOutput {
            destination: finalized.destination().clone(),
            written_bytes: finalized.written_bytes(),
            cleanup_pending: finalized.partial_cleanup_error().is_some(),
        })
    }
}

/// Reads a manifest body, bounded by [`MAX_MANIFEST_BYTES`].
pub(crate) async fn open_manifest(
    client: &HttpClient,
    request: &RequestContext,
) -> Result<(Vec<u8>, Option<String>), DownloadFailure> {
    let mut body = client.open(request).await.map_err(http_failure)?;
    let content_type = body.metadata().content_type().map(str::to_owned);

    let mut bytes = Vec::new();

    while let Some(chunk) = body.next_chunk().await.map_err(http_failure)? {
        let length = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(size_overflow)?;

        if length > MAX_MANIFEST_BYTES {
            return Err(DownloadFailure::new(
                FailureKind::UnsupportedResource,
                "the media manifest is too large to be a manifest",
                false,
            ));
        }

        bytes.extend_from_slice(&chunk);
    }

    Ok((bytes, content_type))
}

/// Builds the request for one segment.
///
/// Headers are only carried over to the same origin as the manifest. Segments
/// are frequently served from a third-party host, and forwarding captured
/// cookies or authorization to a host the user did not configure would leak
/// credentials. Range and validator headers are dropped because they describe
/// the manifest request, not the segment.
pub(crate) fn segment_request(
    url: &Url,
    manifest_url: &Url,
    headers: &[RequestHeader],
) -> RequestContext {
    let headers = if url.origin() == manifest_url.origin() {
        headers
            .iter()
            .filter(|header| {
                !matches!(
                    header.name().to_ascii_lowercase().as_str(),
                    "range" | "if-range" | "host" | "content-length"
                )
            })
            .cloned()
            .collect()
    } else {
        Vec::new()
    };

    RequestContext::new(url.clone(), headers)
}

pub(crate) fn not_textual() -> DownloadFailure {
    DownloadFailure::new(
        FailureKind::InvalidResponse,
        "the media manifest is not valid text",
        false,
    )
}

fn size_overflow() -> DownloadFailure {
    DownloadFailure::new(
        FailureKind::Internal,
        "the staged media size exceeds the supported range",
        false,
    )
}

fn http_failure(error: HttpError) -> DownloadFailure {
    let (kind, retryable) = match error {
        HttpError::UnexpectedStatus(status) if (400..500).contains(&status) => {
            (FailureKind::RemoteRejected, false)
        }
        HttpError::UnexpectedStatus(_)
        | HttpError::Timeout
        | HttpError::Transport
        | HttpError::Redirect => (FailureKind::Network, true),
        HttpError::InvalidRequest | HttpError::Configuration => (FailureKind::Internal, false),
        HttpError::InvalidMetadata
        | HttpError::UnsupportedEncoding
        | HttpError::BodyLengthMismatch
        | HttpError::ByteCountOverflow => (FailureKind::InvalidResponse, false),
    };

    DownloadFailure::new(kind, "a segmented media request failed", retryable)
}

pub(crate) fn manifest_failure(error: ManifestError) -> DownloadFailure {
    match error {
        ManifestError::Unsupported(feature) => {
            DownloadFailure::new(FailureKind::UnsupportedResource, feature.to_string(), false)
        }
        ManifestError::NotAManifest | ManifestError::AmbiguousPlaylist => DownloadFailure::new(
            FailureKind::UnsupportedResource,
            "the resource is not a supported segmented-stream manifest",
            false,
        ),
        ManifestError::Malformed
        | ManifestError::InvalidByteRange
        | ManifestError::InvalidNumber => DownloadFailure::new(
            FailureKind::InvalidResponse,
            "the media manifest is malformed",
            false,
        ),
    }
}

fn staging_failure(error: io::Error) -> DownloadFailure {
    let (kind, retryable) = match error.kind() {
        io::ErrorKind::PermissionDenied => (FailureKind::PermissionDenied, false),
        io::ErrorKind::AlreadyExists => (FailureKind::Storage, false),
        _ => (FailureKind::Storage, true),
    };

    DownloadFailure::new(kind, "could not stage segmented media", retryable)
}

/// A failure that means the engine's own state is wrong, not the origin's.
fn internal_failure(message: &'static str) -> DownloadFailure {
    DownloadFailure::new(FailureKind::Internal, message, false)
}

fn partial_failure(error: PartialDownloadError) -> DownloadFailure {
    match error {
        PartialDownloadError::Http(error) => http_failure(error),
        PartialDownloadError::File {
            kind: io::ErrorKind::PermissionDenied,
            ..
        } => DownloadFailure::new(
            FailureKind::PermissionDenied,
            "staged media cannot be published",
            false,
        ),
        PartialDownloadError::InvalidPartial => DownloadFailure::new(
            FailureKind::Integrity,
            "staged media does not match the staged size",
            false,
        ),
        _ => DownloadFailure::new(
            FailureKind::Storage,
            "staged media cannot be published",
            true,
        ),
    }
}

fn finalization_failure(error: FinalizeError) -> DownloadFailure {
    match error {
        FinalizeError::InvalidFilename => DownloadFailure::new(
            FailureKind::Internal,
            "the destination filename is not usable",
            false,
        ),
        FinalizeError::NameAttemptsExhausted => DownloadFailure::new(
            FailureKind::Storage,
            "no destination name is available",
            false,
        ),
        FinalizeError::DifferentDirectory | FinalizeError::InvalidPartial => DownloadFailure::new(
            FailureKind::Internal,
            "staged media cannot be published",
            false,
        ),
        FinalizeError::File(io::ErrorKind::PermissionDenied) => DownloadFailure::new(
            FailureKind::PermissionDenied,
            "staged media cannot be published",
            false,
        ),
        FinalizeError::File(_) => DownloadFailure::new(
            FailureKind::Storage,
            "staged media cannot be published",
            true,
        ),
    }
}
