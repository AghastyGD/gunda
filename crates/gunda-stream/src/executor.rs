//! Executing a segmented-media plan through the shared download contract.
//!
//! The engine fetches the planned resources in order and appends them to the
//! same staging file a plain HTTP download uses, then publishes it through the
//! same validated `finalize_download` path. Nothing about the job lifecycle,
//! progress reporting, or conflict handling is reimplemented here.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::slice;
use std::str;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gunda_core::application::{
    DownloadCancellation, DownloadControlRequest, DownloadExecutor, ExecutionInput,
    ExecutionOutput, PreparedTransfer, RepositoryErrorKind, StagedTransfer, TransferOutcome,
    TransferProgress,
};
use gunda_core::download::{
    DownloadDestination, DownloadFailure, DownloadId, DownloadJob, DownloadState, FailureKind,
    RequestContext, RequestHeader, ResolvedDestination, ResourceDescriptor, ResourceKind,
};
use gunda_http::{
    FinalizeError, HttpBody, HttpClient, HttpError, HttpRangeBody, PartialDownload,
    PartialDownloadError, finalize_download, partial_path, plan_destination, url_filename,
};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};
use url::Url;

use crate::container::{Container, SNIFF_BYTES, detect};
use crate::dash;
use crate::error::ManifestError;
use crate::hls;
use crate::mux::{MuxError, mux_mp4};
use crate::plan::{HlsOutcome, TrackPlan, plan_dash_selected, plan_hls, plan_hls_selected};
use crate::presentation::{self, Presentation};
use crate::resume::{
    PlanFingerprint, ResumePersistence, StreamResumeState, StreamResumeStore, TrackCheckpoint,
};

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
    resume_store: Option<Arc<dyn ResumePersistence>>,
}

impl StreamExecutor {
    #[must_use]
    pub fn new(client: HttpClient) -> Self {
        Self {
            client: Arc::new(client),
            resume_store: None,
        }
    }

    #[must_use]
    pub fn with_resume_store<S>(client: HttpClient, store: S) -> Self
    where
        S: StreamResumeStore + 'static,
    {
        Self {
            client: Arc::new(client),
            resume_store: Some(Arc::new(store)),
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

    pub async fn can_resume(&self, job: &DownloadJob) -> Result<bool, DownloadFailure> {
        if !matches!(
            job.state(),
            DownloadState::Paused | DownloadState::Interrupted
        ) || !matches!(job.request().url().scheme(), "http" | "https")
            || job.resource().is_some_and(|resource| {
                !matches!(resource.kind(), ResourceKind::Hls | ResourceKind::Dash)
            })
        {
            return Ok(false);
        }

        let Some(store) = &self.resume_store else {
            return Ok(false);
        };
        let Some(state) = store
            .find_state(job.id())
            .await
            .map_err(|_| storage_failure("could not inspect stream resume state"))?
        else {
            return Ok(false);
        };

        let directory = job.destination().directory();

        for (track, checkpoint) in state.tracks().iter().enumerate() {
            let path = staged_path(directory, job.id(), track, state.tracks().len());

            match tokio::fs::symlink_metadata(&path).await {
                Ok(metadata)
                    if metadata.file_type().is_file()
                        && metadata.len() >= checkpoint.durable_bytes() => {}
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && checkpoint.durable_bytes() == 0 => {}
                _ => return Ok(false),
            }
        }

        Ok(true)
    }

    pub async fn reset_for_restart(
        &self,
        id: DownloadId,
        directory: &Path,
    ) -> Result<(), StreamRestartResetError> {
        let track_count = match &self.resume_store {
            Some(store) => store
                .find_state(id)
                .await
                .map_err(|error| StreamRestartResetError::ResumeStorage(error.kind()))?
                .map(|state| state.tracks().len()),
            None => None,
        };

        let mut paths = vec![partial_path(directory, id)];
        if let Some(track_count) = track_count {
            if track_count > 1 {
                paths.extend((0..track_count).map(|track| track_path(directory, id, track as u8)));
            }
        } else {
            // Without persisted state the staging shape is unknown, so both
            // track files are candidates for removal.
            paths.extend([track_path(directory, id, 0), track_path(directory, id, 1)]);
        }

        for path in &paths {
            remove_staging_file(path)
                .await
                .map_err(|error| StreamRestartResetError::StagingFile(error.kind()))?;
        }

        if let Some(store) = &self.resume_store {
            store
                .clear_state(id)
                .await
                .map_err(|error| StreamRestartResetError::ResumeStorage(error.kind()))?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamRestartResetError {
    StagingFile(io::ErrorKind),
    ResumeStorage(RepositoryErrorKind),
}

impl fmt::Display for StreamRestartResetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StagingFile(kind) => write!(f, "could not discard staged media: {kind:?}"),
            Self::ResumeStorage(kind) => {
                write!(f, "could not clear segmented resume state: {kind:?}")
            }
        }
    }
}

impl Error for StreamRestartResetError {}

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
    resume_store: Option<Arc<dyn ResumePersistence>>,
    resume: Option<StreamResumeState>,
}

pub struct StreamStagedTransfer {
    id: DownloadId,
    destination: DownloadDestination,
    /// Fetched bytes; progress is reported against this.
    written_bytes: u64,
    /// Published file length, which can differ after muxing.
    published_bytes: u64,
    content_type: Option<String>,
    resume_store: Option<Arc<dyn ResumePersistence>>,
    staging_paths: Vec<PathBuf>,
}

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
            progress,
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
        let track_count = plan.track_count();
        let fingerprint = PlanFingerprint::of(&plan, request.url());
        let resume = match &self.resume_store {
            Some(store) => {
                let state = match store.find_state(id).await.map_err(|error| {
                    storage_failure_with("could not load segmented resume state", error.kind())
                })? {
                    Some(state) => {
                        if state.fingerprint() != fingerprint || state.tracks().len() != track_count
                        {
                            return Err(recovery_integrity(
                                "saved progress does not match this presentation",
                            ));
                        }
                        validate_track_bounds(&state, &plan)?;
                        state
                    }
                    None => store
                        .initialize_state(id, fingerprint, track_count)
                        .await
                        .map_err(|error| {
                            storage_failure_with(
                                "could not initialize segmented resume state",
                                error.kind(),
                            )
                        })?,
                };

                if let (Some(saved), Some(now)) = (progress.total_bytes(), total_bytes)
                    && saved != now
                {
                    return Err(recovery_integrity(
                        "saved download size does not match the segmented media",
                    ));
                }
                if total_bytes.is_some_and(|total| state.starting_bytes() > total) {
                    return Err(recovery_integrity(
                        "saved progress exceeds the segmented media size",
                    ));
                }

                Some(state)
            }
            None => None,
        };

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
            resume_store: self.resume_store.clone(),
            resume,
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
        let text = str::from_utf8(&bytes).map_err(|_| not_textual())?;

        if !text.trim_start().starts_with("#EXTM3U") {
            let mpd = dash::parse(text).map_err(manifest_failure)?;

            return Ok((
                plan_dash_selected(&base, &mpd, request.selection()).map_err(manifest_failure)?,
                content_type,
            ));
        }

        let playlist = hls::parse(text).map_err(manifest_failure)?;

        match plan_hls_selected(&base, &playlist, request.selection()).map_err(manifest_failure)? {
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
        let text = str::from_utf8(&bytes).map_err(|_| not_textual())?;
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

    fn starting_bytes(&self) -> u64 {
        self.resume
            .as_ref()
            .map_or(0, StreamResumeState::starting_bytes)
    }

    fn supports_pause(&self) -> bool {
        self.resume.is_some()
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
        let mut resume = self.resume.clone();
        let result = self
            .write_track(0, &path, 0, &progress, &cancellation, resume.as_mut())
            .await;
        let (written, stopped) = match result {
            Ok(result) => result,
            Err(failure) => {
                if self.resume.is_none() {
                    let _ = remove_staging_file(&path).await;
                }
                return Err(failure);
            }
        };

        if stopped {
            if cancellation.request_kind() == Some(DownloadControlRequest::Cancel) {
                cleanup_staging(
                    slice::from_ref(&path),
                    self.resume_store.as_deref(),
                    self.id,
                )
                .await;
            }
            return Ok(TransferOutcome::Stopped {
                written_bytes: written,
            });
        }

        Ok(TransferOutcome::Finished(StreamStagedTransfer {
            id: self.id,
            destination: self.destination,
            written_bytes: written,
            published_bytes: written,
            content_type: self.content_type,
            resume_store: self.resume_store,
            staging_paths: Vec::new(),
        }))
    }
}

impl StreamPreparedTransfer {
    /// Fetches every track and combines them into the one published file.
    ///
    /// Separate tracks cannot be concatenated: only muxing produces a file a
    /// player can use. Each track is therefore fetched into a staging file of
    /// its own and the two are then written into the partial file the shared
    /// machinery publishes from. Staging is removed after publication; a pause
    /// keeps it so the next attempt can continue.
    async fn transfer_muxed(
        self,
        progress: TransferProgress,
        cancellation: DownloadCancellation,
    ) -> Result<TransferOutcome<StreamStagedTransfer>, DownloadFailure> {
        let directory = self.destination.directory().to_path_buf();
        let track_count = self.plan.track_count();
        let paths: Vec<PathBuf> = (0..track_count)
            .map(|track| track_path(&directory, self.id, track as u8))
            .collect();
        let mut resume = self.resume.clone();
        let mut written = 0_u64;

        for (track, path) in paths.iter().enumerate() {
            let result = self
                .write_track(
                    track as u8,
                    path,
                    written,
                    &progress,
                    &cancellation,
                    resume.as_mut(),
                )
                .await;
            let (bytes, stopped) = match result {
                Ok(result) => result,
                Err(failure) => {
                    if self.resume.is_none() {
                        cleanup_staging(&paths, None, self.id).await;
                    }
                    return Err(failure);
                }
            };

            written = written.checked_add(bytes).ok_or_else(size_overflow)?;

            if stopped {
                if cancellation.request_kind() == Some(DownloadControlRequest::Cancel) {
                    cleanup_staging(&paths, self.resume_store.as_deref(), self.id).await;
                }
                return Ok(TransferOutcome::Stopped {
                    written_bytes: written,
                });
            }
        }

        let (Some(video), Some(audio)) = (paths.first(), paths.get(1)) else {
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
            written_bytes: written,
            published_bytes: combined,
            content_type: self.content_type,
            resume_store: self.resume_store,
            staging_paths: paths,
        }))
    }

    /// Fetches one track's resources into its own staging file.
    ///
    /// Returns the track's total staged bytes and whether the transfer was
    /// stopped; `base` is what the progress counts from, so a second track
    /// continues the count rather than restarting it.
    async fn write_track(
        &self,
        track: u8,
        path: &Path,
        base: u64,
        progress: &TransferProgress,
        cancellation: &DownloadCancellation,
        mut resume: Option<&mut StreamResumeState>,
    ) -> Result<(u64, bool), DownloadFailure> {
        let checkpoint = resume
            .as_ref()
            .map(|state| state.tracks()[usize::from(track)])
            .unwrap_or_else(TrackCheckpoint::start);
        let resources: Vec<_> = self.plan.resources_of(track).collect();

        if checkpoint.next_resource() > resources.len() {
            return Err(recovery_integrity(
                "saved segment position exceeds the presentation",
            ));
        }

        let mut written = checkpoint.durable_bytes();
        let mut next_resource = checkpoint.next_resource();
        let mut file = open_track(path, written, resume.is_some()).await?;
        let mut last_checkpoint_bytes = written;
        let mut last_checkpoint_at = Instant::now();

        for (index, resource) in resources.iter().enumerate().skip(next_resource) {
            if cancellation.is_requested() {
                checkpoint_track(
                    &mut file,
                    self.id,
                    self.resume_store.as_deref(),
                    &mut resume,
                    track,
                    next_resource,
                    written,
                )
                .await?;
                return Ok((written, true));
            }

            let request = segment_request(resource.url(), &self.manifest_url, &self.headers);
            let resource_start = written;

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

            loop {
                let next = tokio::select! {
                    biased;

                    _ = cancellation.stopped() => None,
                    result = body.write_next(&mut file) => Some(result),
                };

                match next {
                    None => {
                        truncate_track(&mut file, resource_start).await?;
                        checkpoint_track(
                            &mut file,
                            self.id,
                            self.resume_store.as_deref(),
                            &mut resume,
                            track,
                            next_resource,
                            resource_start,
                        )
                        .await?;
                        return Ok((resource_start, true));
                    }
                    Some(Err(error)) => {
                        truncate_track(&mut file, resource_start).await?;
                        checkpoint_track(
                            &mut file,
                            self.id,
                            self.resume_store.as_deref(),
                            &mut resume,
                            track,
                            next_resource,
                            resource_start,
                        )
                        .await?;
                        return Err(error);
                    }
                    Some(Ok(None)) => break,
                    Some(Ok(Some(length))) => {
                        written = written.checked_add(length).ok_or_else(size_overflow)?;
                        progress
                            .report_written(base.checked_add(written).ok_or_else(size_overflow)?);
                    }
                }
            }

            next_resource = index + 1;
            let checkpoint_due = next_resource == resources.len()
                || written.saturating_sub(last_checkpoint_bytes) >= 8 * 1024 * 1024
                || last_checkpoint_at.elapsed() >= Duration::from_secs(1);

            if checkpoint_due {
                checkpoint_track(
                    &mut file,
                    self.id,
                    self.resume_store.as_deref(),
                    &mut resume,
                    track,
                    next_resource,
                    written,
                )
                .await?;
                last_checkpoint_bytes = written;
                last_checkpoint_at = Instant::now();
            }
        }

        checkpoint_track(
            &mut file,
            self.id,
            self.resume_store.as_deref(),
            &mut resume,
            track,
            next_resource,
            written,
        )
        .await?;

        Ok((written, false))
    }
}

fn track_path(directory: &Path, id: DownloadId, track: u8) -> PathBuf {
    let partial = partial_path(directory, id);
    let mut name = partial.file_name().unwrap_or_default().to_owned();

    name.push(if track == 1 { ".audio" } else { ".video" });

    partial.with_file_name(name)
}

fn staged_path(directory: &Path, id: DownloadId, track: usize, track_count: usize) -> PathBuf {
    if track_count == 1 {
        partial_path(directory, id)
    } else {
        track_path(directory, id, track as u8)
    }
}

async fn open_track(
    path: &Path,
    durable_bytes: u64,
    resuming: bool,
) -> Result<File, DownloadFailure> {
    if !resuming {
        return OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
            .map_err(staging_failure);
    }

    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(recovery_integrity(
                "staged segmented media is not a regular file",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && durable_bytes == 0 => {
            return OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .await
                .map_err(staging_failure);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(recovery_integrity("staged segmented media is missing"));
        }
        Err(error) => return Err(staging_failure(error)),
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .await
        .map_err(staging_failure)?;
    let length = file.metadata().await.map_err(staging_failure)?.len();

    if length < durable_bytes {
        return Err(recovery_integrity(
            "staged segmented media is shorter than saved progress",
        ));
    }
    if length > durable_bytes {
        file.set_len(durable_bytes).await.map_err(staging_failure)?;
    }

    file.seek(SeekFrom::End(0)).await.map_err(staging_failure)?;

    Ok(file)
}

async fn truncate_track(file: &mut File, bytes: u64) -> Result<(), DownloadFailure> {
    file.flush().await.map_err(staging_failure)?;
    file.set_len(bytes).await.map_err(staging_failure)?;
    file.seek(SeekFrom::End(0)).await.map_err(staging_failure)?;

    Ok(())
}

async fn checkpoint_track(
    file: &mut File,
    id: DownloadId,
    store: Option<&dyn ResumePersistence>,
    resume: &mut Option<&mut StreamResumeState>,
    track: u8,
    next_resource: usize,
    durable_bytes: u64,
) -> Result<(), DownloadFailure> {
    file.flush().await.map_err(staging_failure)?;
    file.sync_all().await.map_err(staging_failure)?;

    let Some(state) = resume.as_deref_mut() else {
        return Ok(());
    };
    let store = store.ok_or_else(|| {
        storage_failure_with(
            "segmented resume state is unavailable",
            RepositoryErrorKind::InvalidData,
        )
    })?;

    let candidate = state
        .with_track(
            usize::from(track),
            TrackCheckpoint::new(next_resource, durable_bytes),
        )
        .map_err(|_| recovery_integrity("segmented checkpoint cannot move backwards"))?;
    let persisted = store
        .save_state(id, candidate.clone())
        .await
        .map_err(|error| storage_failure_with("could not save segmented progress", error.kind()))?;

    if persisted != candidate {
        return Err(storage_failure_with(
            "segmented progress was not stored as written",
            RepositoryErrorKind::InvalidData,
        ));
    }

    *state = persisted;

    Ok(())
}

async fn remove_staging_file(path: &Path) -> io::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_file() => tokio::fs::remove_file(path).await,
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staging path is not a regular file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn cleanup_staging(paths: &[PathBuf], store: Option<&dyn ResumePersistence>, id: DownloadId) {
    for path in paths {
        let _ = remove_staging_file(path).await;
    }

    if let Some(store) = store {
        let _ = store.clear_state(id).await;
    }
}

fn validate_track_bounds(
    state: &StreamResumeState,
    plan: &TrackPlan,
) -> Result<(), DownloadFailure> {
    for (track, checkpoint) in state.tracks().iter().enumerate() {
        if checkpoint.next_resource() > plan.resources_of(track as u8).count() {
            return Err(recovery_integrity(
                "saved segment position exceeds the presentation",
            ));
        }
    }

    Ok(())
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
        MuxError::Io(_) => staging_failure(io::Error::other("muxing")),
    }
}

impl StagedTransfer for StreamStagedTransfer {
    fn written_bytes(&self) -> u64 {
        self.written_bytes
    }

    async fn finalize(self) -> Result<ExecutionOutput, DownloadFailure> {
        let Self {
            id,
            destination,
            written_bytes,
            published_bytes,
            content_type,
            resume_store,
            staging_paths,
        } = self;
        let partial =
            PartialDownload::adopt(id, destination.directory(), published_bytes, content_type)
                .await
                .map_err(partial_failure)?;

        let finalized = finalize_download(partial, &destination)
            .await
            .map_err(|failure| finalization_failure(failure.error()))?;

        if let Some(store) = &resume_store {
            let _ = store.clear_state(id).await;
        }
        for path in &staging_paths {
            let _ = remove_staging_file(path).await;
        }

        Ok(ExecutionOutput {
            destination: finalized.destination().clone(),
            written_bytes,
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

fn recovery_integrity(message: &'static str) -> DownloadFailure {
    DownloadFailure::new(FailureKind::Integrity, message, false)
}

fn storage_failure(message: &'static str) -> DownloadFailure {
    storage_failure_with(message, RepositoryErrorKind::Internal)
}

fn storage_failure_with(message: &'static str, kind: RepositoryErrorKind) -> DownloadFailure {
    let retryable = matches!(
        kind,
        RepositoryErrorKind::Internal | RepositoryErrorKind::Unavailable
    );

    DownloadFailure::new(FailureKind::Storage, message, retryable)
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
        ManifestError::SelectionUnavailable => DownloadFailure::new(
            FailureKind::InvalidResponse,
            "the selected quality is no longer available",
            false,
        ),
        ManifestError::TooManySegments => DownloadFailure::new(
            FailureKind::UnsupportedResource,
            "the media manifest contains too many segments",
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
