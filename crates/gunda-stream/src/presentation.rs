//! Describing what a presentation offers before any of it is downloaded.
//!
//! A capture reports one URL, but a presentation usually carries several
//! qualities and tracks, and the browser extension cannot know which one the
//! user wants. Inspection reads the manifest — and, where a playlist has to be
//! read to describe a track, that playlist — so the choice can be made before a
//! byte of media is fetched.

use std::cmp::Reverse;
use std::str;

use gunda_core::download::{DownloadFailure, ExecutionSelection, RequestContext, RequestHeader};
use gunda_http::HttpClient;
use url::Url;

use crate::container::Container;
use crate::dash::{self, TrackKind};
use crate::executor::{
    manifest_failure, not_textual, open_manifest, output_filename, segment_request,
};
use crate::hls::{self, Playlist};
use crate::plan::{
    HlsOutcome, TrackPlan, audio_set_index, bandwidth_hint, best_representation_index,
    label_for_variant, plan_dash_selected, plan_hls, select_set_index,
};
use crate::selection::{dash_selection, hls_selection};

/// Most variants described by one inspection.
///
/// Describing a variant means reading its playlist, and the number of variants
/// is remote input, so the work is bounded the same way the plan itself is.
const MAX_DESCRIBED_VARIANTS: usize = 24;

#[derive(Debug, Clone)]
pub struct Presentation {
    tracks: Vec<PresentationTrack>,
}

impl Presentation {
    /// Best first.
    #[must_use]
    pub fn tracks(&self) -> &[PresentationTrack] {
        &self.tracks
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct PresentationTrack {
    label: String,
    kind: TrackKind,
    url: Url,
    filename: Option<String>,
    resolution: Option<(u32, u32)>,
    bandwidth: Option<u64>,
    codecs: Option<String>,
    duration_millis: Option<u64>,
    size: TrackSize,
    container: Option<Container>,
    selection: Option<ExecutionSelection>,
}

impl PresentationTrack {
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    #[must_use]
    pub const fn kind(&self) -> TrackKind {
        self.kind
    }

    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The name the track is published under, when one is known.
    ///
    /// Always taken from the presentation rather than from the track's own
    /// playlist: a variant playlist is frequently called `index.m3u8`, and
    /// naming the download after it would lose the name of what was downloaded.
    #[must_use]
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    #[must_use]
    pub const fn resolution(&self) -> Option<(u32, u32)> {
        self.resolution
    }

    #[must_use]
    pub const fn bandwidth(&self) -> Option<u64> {
        self.bandwidth
    }

    #[must_use]
    pub fn codecs(&self) -> Option<&str> {
        self.codecs.as_deref()
    }

    #[must_use]
    pub const fn duration_millis(&self) -> Option<u64> {
        self.duration_millis
    }

    #[must_use]
    pub const fn size(&self) -> TrackSize {
        self.size
    }

    #[must_use]
    pub const fn container(&self) -> Option<Container> {
        self.container
    }

    /// Core stores it; the stream engine interprets it.
    #[must_use]
    pub const fn selection(&self) -> Option<&ExecutionSelection> {
        self.selection.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSize {
    /// Every part declares its length, so the total is exactly known.
    Exact(u64),
    /// Derived from the declared bitrate and duration, so it is only as good as
    /// those two numbers.
    Estimated(u64),
    /// Neither the manifest nor the parts say.
    Unknown,
}

impl TrackSize {
    #[must_use]
    pub const fn bytes(self) -> Option<u64> {
        match self {
            Self::Exact(total) | Self::Estimated(total) => Some(total),
            Self::Unknown => None,
        }
    }

    /// Whether the byte count is exact rather than estimated.
    #[must_use]
    pub const fn is_exact(self) -> bool {
        matches!(self, Self::Exact(_))
    }
}

/// Nothing is downloaded: the manifest is read, and each variant's playlist is
/// read to report its duration and size. A presentation whose entry point is
/// already one media playlist has exactly one track.
pub async fn inspect(
    client: &HttpClient,
    request: &RequestContext,
) -> Result<Presentation, DownloadFailure> {
    let (bytes, _) = open_manifest(client, request).await?;
    let base = request.url().clone();
    let text = str::from_utf8(&bytes).map_err(|_| not_textual())?;

    if !text.trim_start().starts_with("#EXTM3U") {
        let mpd = dash::parse(text).map_err(manifest_failure)?;

        return described_dash_presentation(client, request, &base, &mpd).await;
    }

    let playlist = hls::parse(text).map_err(manifest_failure)?;

    match &playlist {
        Playlist::Media(_) => Ok(Presentation {
            tracks: vec![described_playlist(&base, &playlist)],
        }),
        Playlist::Master(master) => {
            let mut tracks = Vec::new();

            for (index, variant) in master
                .variants()
                .iter()
                .take(MAX_DESCRIBED_VARIANTS)
                .enumerate()
            {
                if let Some(track) =
                    described_variant(client, request, &base, master, index, variant).await
                {
                    tracks.push(track);
                }
            }

            for rendition in master.renditions() {
                // Muxed renditions have no playlist of their own: their audio is
                // inside the variants, and subtitles are not media Gunda can
                // publish.
                if !rendition.kind().eq_ignore_ascii_case("AUDIO") {
                    continue;
                }

                let Some(uri) = rendition.uri() else {
                    continue;
                };

                let Ok(url) = base.join(uri) else {
                    continue;
                };

                let mut track = PresentationTrack {
                    label: rendition_label(rendition),
                    kind: TrackKind::Audio,
                    url: url.clone(),
                    filename: output_filename(&base, None),
                    resolution: None,
                    bandwidth: None,
                    codecs: None,
                    duration_millis: None,
                    size: TrackSize::Unknown,
                    container: None,
                    selection: None,
                };

                enrich(client, &base, &url, request.headers(), &mut track).await;

                tracks.push(track);
            }

            sort_tracks(&mut tracks);

            Ok(Presentation { tracks })
        }
    }
}

async fn described_variant(
    client: &HttpClient,
    request: &RequestContext,
    manifest: &Url,
    master: &hls::MasterPlaylist,
    index: usize,
    variant: &hls::Variant,
) -> Option<PresentationTrack> {
    let playlist_url = manifest.join(variant.uri()).ok()?;
    let audio_url = master
        .default_audio_rendition(variant)
        .and_then(hls::Rendition::uri)
        .and_then(|uri| manifest.join(uri).ok());
    let selection = hls_selection(index, variant).ok()?;
    let mut track = PresentationTrack {
        label: label_for_variant(variant).unwrap_or_else(|| format!("Variant {}", index + 1)),
        kind: TrackKind::Video,
        url: manifest.clone(),
        filename: output_filename(manifest, None),
        resolution: variant.resolution(),
        bandwidth: variant.bandwidth(),
        codecs: variant.codecs().map(str::to_owned),
        duration_millis: None,
        size: TrackSize::Unknown,
        container: None,
        selection: Some(selection),
    };

    enrich(
        client,
        manifest,
        &playlist_url,
        request.headers(),
        &mut track,
    )
    .await;

    if let Some(audio_url) = audio_url {
        let mut audio = PresentationTrack {
            label: String::new(),
            kind: TrackKind::Audio,
            url: audio_url.clone(),
            filename: output_filename(manifest, None),
            resolution: None,
            bandwidth: None,
            codecs: None,
            duration_millis: None,
            size: TrackSize::Unknown,
            container: None,
            selection: None,
        };

        enrich(client, manifest, &audio_url, request.headers(), &mut audio).await;

        track.size = combine_sizes(track.size, audio.size);
        track.duration_millis = track.duration_millis.max(audio.duration_millis);
        track.label = format!("{} with audio", track.label);
    }

    track.filename = output_filename(manifest, track.container);

    Some(track)
}

async fn described_dash_presentation(
    _client: &HttpClient,
    _request: &RequestContext,
    base: &Url,
    mpd: &dash::Mpd,
) -> Result<Presentation, DownloadFailure> {
    let period = mpd.media_period().map_err(manifest_failure)?;

    if mpd.kind() == dash::MpdKind::Dynamic {
        return Err(manifest_failure(
            crate::error::UnsupportedFeature::IncompletePresentation.into(),
        ));
    }

    let duration = period.duration_millis().or_else(|| mpd.duration_millis());
    let mut video_set = None;
    let mut audio_set = audio_set_index(period);

    if let Some(index) = select_set_index(period) {
        if period.adaptation_sets()[index].kind() == TrackKind::Video {
            video_set = Some(index);
        } else {
            audio_set = Some(index);
        }
    }

    if video_set.is_none() && audio_set.is_none() {
        return Err(manifest_failure(crate::error::ManifestError::Malformed));
    }

    let mut tracks = Vec::new();
    let audio_choice = audio_set.and_then(|set| {
        best_representation_index(&period.adaptation_sets()[set]).map(|representation| {
            (
                set,
                representation,
                period.adaptation_sets()[set].representations()[representation].bandwidth(),
            )
        })
    });

    if let Some(set_index) = video_set {
        let set = &period.adaptation_sets()[set_index];

        for (index, representation) in set
            .representations()
            .iter()
            .take(MAX_DESCRIBED_VARIANTS)
            .enumerate()
        {
            let selection = dash_selection(
                Some((set_index, index, representation.bandwidth())),
                audio_choice,
            )
            .map_err(manifest_failure)?;
            let plan = plan_dash_selected(base, mpd, Some(&selection)).ok();
            let mut track = described_dash_track(DashTrackDescription {
                base,
                representation,
                kind: TrackKind::Video,
                label: dash_video_label(representation),
                selection,
                plan: plan.as_ref(),
                duration,
                estimate_bandwidth: combined_bandwidth(representation.bandwidth(), audio_choice),
            });

            if audio_choice.is_some() {
                track.label = format!("{} with audio", track.label);
            }

            tracks.push(track);
        }
    }

    if let Some(set_index) = audio_set {
        let set = &period.adaptation_sets()[set_index];

        for (index, representation) in set
            .representations()
            .iter()
            .take(MAX_DESCRIBED_VARIANTS)
            .enumerate()
        {
            let selection =
                dash_selection(None, Some((set_index, index, representation.bandwidth())))
                    .map_err(manifest_failure)?;
            let plan = plan_dash_selected(base, mpd, Some(&selection)).ok();

            tracks.push(described_dash_track(DashTrackDescription {
                base,
                representation,
                kind: TrackKind::Audio,
                label: dash_audio_label(set, representation),
                selection,
                plan: plan.as_ref(),
                duration,
                estimate_bandwidth: representation.bandwidth(),
            }));
        }
    }

    sort_tracks(&mut tracks);

    Ok(Presentation { tracks })
}

/// Puts video qualities first, best first, and keeps audio-only tracks after.
fn sort_tracks(tracks: &mut [PresentationTrack]) {
    tracks.sort_by_key(|track| {
        (
            u8::from(track.kind() != TrackKind::Video),
            Reverse(track.bandwidth().unwrap_or(0)),
        )
    });
}

struct DashTrackDescription<'a> {
    base: &'a Url,
    representation: &'a dash::Representation,
    kind: TrackKind,
    label: String,
    selection: ExecutionSelection,
    plan: Option<&'a TrackPlan>,
    duration: Option<u64>,
    estimate_bandwidth: Option<u64>,
}

fn described_dash_track(description: DashTrackDescription<'_>) -> PresentationTrack {
    let DashTrackDescription {
        base,
        representation,
        kind,
        label,
        selection,
        plan,
        duration,
        estimate_bandwidth,
    } = description;
    let container = plan.and_then(TrackPlan::container_hint);
    let duration_millis = plan.and_then(TrackPlan::duration_millis).or(duration);
    let size = plan
        .and_then(TrackPlan::known_total_bytes)
        .map(TrackSize::Exact)
        .unwrap_or_else(|| match (estimate_bandwidth, duration_millis) {
            (Some(bandwidth), Some(duration)) => {
                TrackSize::Estimated(bandwidth.saturating_mul(duration) / 8000)
            }
            _ => TrackSize::Unknown,
        });

    PresentationTrack {
        label,
        kind,
        url: base.clone(),
        filename: output_filename(base, container),
        resolution: representation.resolution(),
        bandwidth: representation.bandwidth(),
        codecs: representation.codecs().map(str::to_owned),
        duration_millis,
        size,
        container,
        selection: Some(selection),
    }
}

fn combined_bandwidth(
    video: Option<u64>,
    audio: Option<(usize, usize, Option<u64>)>,
) -> Option<u64> {
    match (video, audio.and_then(|(_, _, bandwidth)| bandwidth)) {
        (Some(video), Some(audio)) => Some(video.saturating_add(audio)),
        (Some(video), None) => Some(video),
        (None, Some(audio)) => Some(audio),
        (None, None) => None,
    }
}

fn dash_video_label(representation: &dash::Representation) -> String {
    match (representation.resolution(), representation.id()) {
        (Some((width, height)), Some(id)) => format!("{width}x{height} ({id})"),
        (Some((width, height)), None) => format!("{width}x{height}"),
        (None, Some(id)) => id.to_owned(),
        (None, None) => "Video".to_owned(),
    }
}

fn dash_audio_label(set: &dash::AdaptationSet, representation: &dash::Representation) -> String {
    let label = set.language().map_or_else(
        || "Audio".to_owned(),
        |language| format!("Audio ({language})"),
    );

    match representation.bandwidth() {
        Some(bandwidth) => format!("{label} · {}", bandwidth_hint(bandwidth)),
        None => label,
    }
}

/// A sum is only exact when both halves are; anything less would report a
/// precision the parts do not have.
fn combine_sizes(video: TrackSize, audio: TrackSize) -> TrackSize {
    let exact = video.is_exact() && audio.is_exact();

    match (video.bytes(), audio.bytes()) {
        (Some(video), Some(audio)) => {
            let total = video.saturating_add(audio);

            if exact {
                TrackSize::Exact(total)
            } else {
                TrackSize::Estimated(total)
            }
        }
        _ => TrackSize::Unknown,
    }
}

/// The playlist has already been parsed by the caller, so it is planned from
/// that rather than read a second time.
fn described_playlist(base: &Url, playlist: &Playlist) -> PresentationTrack {
    match plan_hls(base, playlist) {
        Ok(HlsOutcome::Ready(plan)) => track_from_plan(base, base, &plan),
        // The playlist is still one selectable track: a plan that refused it
        // reports its reason when the track is actually downloaded, and hiding
        // the option would leave the user with nothing to choose.
        _ => PresentationTrack {
            label: kind_label(TrackKind::Unknown).to_owned(),
            kind: TrackKind::Unknown,
            url: base.clone(),
            filename: output_filename(base, None),
            resolution: None,
            bandwidth: None,
            codecs: None,
            duration_millis: None,
            size: TrackSize::Unknown,
            container: None,
            selection: None,
        },
    }
}

fn track_from_plan(manifest: &Url, url: &Url, plan: &TrackPlan) -> PresentationTrack {
    let container = plan.container_hint();
    let kind = kind_of(container);

    PresentationTrack {
        label: plan
            .label()
            .map_or_else(|| kind_label(kind).to_owned(), str::to_owned),
        kind,
        url: url.clone(),
        filename: output_filename(manifest, container),
        resolution: None,
        bandwidth: None,
        codecs: None,
        duration_millis: plan.duration_millis(),
        size: size_of(plan, None, plan.duration_millis()),
        container,
        selection: None,
    }
}

/// A playlist that cannot be read or planned leaves the track described by its
/// variant declaration alone. That is deliberate: the download reports the
/// reason a track cannot be fetched, and hiding an option the user might still
/// want would be worse than listing one that may fail.
async fn enrich(
    client: &HttpClient,
    manifest: &Url,
    playlist_url: &Url,
    headers: &[RequestHeader],
    track: &mut PresentationTrack,
) {
    let request = segment_request(playlist_url, manifest, headers);

    let Ok((bytes, _)) = open_manifest(client, &request).await else {
        return;
    };
    let Ok(text) = str::from_utf8(&bytes) else {
        return;
    };
    let Ok(playlist) = hls::parse(text) else {
        return;
    };
    let Ok(HlsOutcome::Ready(plan)) = plan_hls(playlist_url, &playlist) else {
        return;
    };

    track.duration_millis = plan.duration_millis();
    track.container = plan.container_hint();
    track.filename = output_filename(manifest, track.container);
    track.size = size_of(&plan, track.bandwidth, track.duration_millis);
}

fn size_of(plan: &TrackPlan, bandwidth: Option<u64>, duration_millis: Option<u64>) -> TrackSize {
    if let Some(total) = plan.known_total_bytes() {
        return TrackSize::Exact(total);
    }

    match (bandwidth, duration_millis) {
        // Bits per second and milliseconds give bytes at 8000 bits per
        // millisecond, which is the same as the declared average bitrate over
        // the declared duration.
        (Some(bandwidth), Some(duration)) => {
            TrackSize::Estimated(bandwidth.saturating_mul(duration) / 8000)
        }
        _ => TrackSize::Unknown,
    }
}

fn kind_of(container: Option<Container>) -> TrackKind {
    match container {
        Some(Container::M4a | Container::Aac | Container::Mp3) => TrackKind::Audio,
        Some(Container::Mp4 | Container::Ts | Container::Matroska | Container::Webm) => {
            TrackKind::Video
        }
        None => TrackKind::Unknown,
    }
}

fn kind_label(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "Video",
        TrackKind::Audio => "Audio",
        TrackKind::Text => "Text",
        TrackKind::Unknown => "Media",
    }
}

/// Prefers the name and language the manifest gives the rendition.
fn rendition_label(rendition: &hls::Rendition) -> String {
    match (rendition.name(), rendition.language()) {
        (Some(name), Some(language)) => format!("{name} ({language})"),
        (Some(name), None) => name.to_owned(),
        (None, Some(language)) => format!("Audio ({language})"),
        (None, None) => "Audio".to_owned(),
    }
}
