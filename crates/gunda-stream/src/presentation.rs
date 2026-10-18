//! Describing what a presentation offers before any of it is downloaded.
//!
//! A capture reports one URL, but a presentation usually carries several
//! qualities and tracks, and the browser extension cannot know which one the
//! user wants. Inspection reads the manifest — and, where a playlist has to be
//! read to describe a track, that playlist — so the choice can be made before a
//! byte of media is fetched.

use gunda_core::download::{DownloadFailure, RequestContext, RequestHeader};
use gunda_http::HttpClient;
use url::Url;

use crate::container::Container;
use crate::dash::{self, TrackKind};
use crate::executor::{
    manifest_failure, not_textual, open_manifest, output_filename, segment_request,
};
use crate::hls::{self, Playlist};
use crate::plan::{HlsOutcome, TrackPlan, label_for_variant, plan_dash, plan_hls};

/// Most variants described by one inspection.
///
/// Describing a variant means reading its playlist, and the number of variants
/// is remote input, so the work is bounded the same way the plan itself is.
const MAX_DESCRIBED_VARIANTS: usize = 24;

/// What a presentation offers.
#[derive(Debug, Clone)]
pub struct Presentation {
    tracks: Vec<PresentationTrack>,
}

impl Presentation {
    /// The tracks the user can choose between, best first.
    #[must_use]
    pub fn tracks(&self) -> &[PresentationTrack] {
        &self.tracks
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }
}

/// One quality or track of a presentation.
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
}

impl PresentationTrack {
    /// A short description of the track for the user to choose by.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether the track carries video, audio or neither.
    #[must_use]
    pub const fn kind(&self) -> TrackKind {
        self.kind
    }

    /// The URL that downloads exactly this track.
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

    /// The declared frame size, when the manifest gives one.
    #[must_use]
    pub const fn resolution(&self) -> Option<(u32, u32)> {
        self.resolution
    }

    /// The declared bitrate in bits per second, when the manifest gives one.
    #[must_use]
    pub const fn bandwidth(&self) -> Option<u64> {
        self.bandwidth
    }

    /// The declared codec list, when the manifest gives one.
    #[must_use]
    pub fn codecs(&self) -> Option<&str> {
        self.codecs.as_deref()
    }

    /// How long the track plays for.
    #[must_use]
    pub const fn duration_millis(&self) -> Option<u64> {
        self.duration_millis
    }

    /// What is known about the track's final size.
    #[must_use]
    pub const fn size(&self) -> TrackSize {
        self.size
    }

    /// The container the track's media is in, when it is known.
    #[must_use]
    pub const fn container(&self) -> Option<Container> {
        self.container
    }
}

/// What is known about a track's final size.
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
    /// The byte count, when there is one.
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

/// Reads a presentation and describes the tracks it offers.
///
/// Nothing is downloaded: the manifest is read, and each variant's playlist is
/// read to report its duration and size. A presentation whose entry point is
/// already one media playlist has exactly one track.
pub async fn inspect(
    client: &HttpClient,
    request: &RequestContext,
) -> Result<Presentation, DownloadFailure> {
    let (bytes, _) = open_manifest(client, request).await?;
    let base = request.url().clone();
    let text = std::str::from_utf8(&bytes).map_err(|_| not_textual())?;

    if !text.trim_start().starts_with("#EXTM3U") {
        let mpd = dash::parse(text).map_err(manifest_failure)?;
        let plan = plan_dash(&base, &mpd).map_err(manifest_failure)?;

        // A DASH presentation is addressed as a whole and the plan has already
        // chosen the track, so this describes what a download would produce
        // rather than a menu of representations.
        return Ok(Presentation {
            tracks: vec![track_from_plan(&base, &base, &plan)],
        });
    }

    let playlist = hls::parse(text).map_err(manifest_failure)?;

    match &playlist {
        Playlist::Media(_) => Ok(Presentation {
            tracks: vec![described_playlist(&base, &playlist)],
        }),
        Playlist::Master(master) => {
            let mut tracks = Vec::new();

            // A variant whose audio is a rendition of its own carries no sound
            // itself, and only the presentation knows which rendition belongs
            // with which quality. Such a presentation is offered as a whole:
            // its qualities cannot be downloaded one by one without each of
            // them losing its sound.
            let split = master
                .variants()
                .iter()
                .any(|variant| master.default_audio_rendition(variant).is_some());

            if split {
                if let Some(track) = described_presentation(&base, client, request, master).await {
                    tracks.push(track);
                }
            } else {
                for (index, variant) in master
                    .variants()
                    .iter()
                    .take(MAX_DESCRIBED_VARIANTS)
                    .enumerate()
                {
                    let Ok(url) = base.join(variant.uri()) else {
                        continue;
                    };

                    let mut track = PresentationTrack {
                        label: label_for_variant(variant)
                            .unwrap_or_else(|| format!("Variant {}", index + 1)),
                        kind: TrackKind::Video,
                        url,
                        filename: output_filename(&base, None),
                        resolution: variant.resolution(),
                        bandwidth: variant.bandwidth(),
                        codecs: variant.codecs().map(str::to_owned),
                        duration_millis: None,
                        size: TrackSize::Unknown,
                        container: None,
                    };

                    enrich(client, &base, request.headers(), &mut track).await;

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
                    url,
                    filename: output_filename(&base, None),
                    resolution: None,
                    bandwidth: None,
                    codecs: None,
                    duration_millis: None,
                    size: TrackSize::Unknown,
                    container: None,
                };

                enrich(client, &base, request.headers(), &mut track).await;

                tracks.push(track);
            }

            Ok(Presentation { tracks })
        }
    }
}

/// Describes a presentation whose variants carry no audio of their own.
///
/// The best variant is read to report what the presentation amounts to, and the
/// audio rendition that belongs with it is read too, because both are part of
/// the download and the size of one without the other would be a lie.
async fn described_presentation(
    base: &Url,
    client: &HttpClient,
    request: &RequestContext,
    master: &hls::MasterPlaylist,
) -> Option<PresentationTrack> {
    let variant = master.best_variant()?;
    let headers = request.headers();

    let mut track = PresentationTrack {
        label: label_for_variant(variant).unwrap_or_else(|| "Best quality".to_owned()),
        kind: TrackKind::Video,
        url: base.join(variant.uri()).ok()?,
        filename: output_filename(base, None),
        resolution: variant.resolution(),
        bandwidth: variant.bandwidth(),
        codecs: variant.codecs().map(str::to_owned),
        duration_millis: None,
        size: TrackSize::Unknown,
        container: None,
    };

    enrich(client, base, headers, &mut track).await;

    if let Some(uri) = master
        .default_audio_rendition(variant)
        .and_then(hls::Rendition::uri)
    {
        let mut audio = PresentationTrack {
            label: String::new(),
            kind: TrackKind::Audio,
            url: base.join(uri).ok()?,
            filename: output_filename(base, None),
            resolution: None,
            bandwidth: None,
            codecs: None,
            duration_millis: None,
            size: TrackSize::Unknown,
            container: None,
        };

        enrich(client, base, headers, &mut audio).await;

        track.size = combine_sizes(track.size, audio.size);
        track.duration_millis = track.duration_millis.max(audio.duration_millis);
    }

    // The download is the whole presentation, because a variant's own playlist
    // would fetch the picture without its sound.
    track.label = format!("{} with audio", track.label);
    track.url = base.clone();
    track.filename = output_filename(base, track.container);

    Some(track)
}

/// Combines the size of a picture with the size of the sound that belongs with
/// it.
///
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

/// Describes a presentation whose entry point is a single playlist.
///
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
        },
    }
}

/// Describes what a plan would download, as one selectable track.
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
    }
}

/// Reads a variant's media playlist and fills in what it says about the track.
///
/// A playlist that cannot be read or planned leaves the track described by its
/// variant declaration alone. That is deliberate: the download reports the
/// reason a track cannot be fetched, and hiding an option the user might still
/// want would be worse than listing one that may fail.
async fn enrich(
    client: &HttpClient,
    base: &Url,
    headers: &[RequestHeader],
    track: &mut PresentationTrack,
) {
    let request = segment_request(&track.url, base, headers);

    let Ok((bytes, _)) = open_manifest(client, &request).await else {
        return;
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return;
    };
    let Ok(playlist) = hls::parse(text) else {
        return;
    };
    let Ok(HlsOutcome::Ready(plan)) = plan_hls(&track.url, &playlist) else {
        return;
    };

    track.duration_millis = plan.duration_millis();
    track.container = plan.container_hint();
    track.filename = output_filename(base, track.container);
    track.size = size_of(&plan, track.bandwidth, track.duration_millis);
}

/// What a track's size is, given what the plan and the bitrate say.
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

/// The media kind a container implies.
fn kind_of(container: Option<Container>) -> TrackKind {
    match container {
        Some(Container::M4a | Container::Aac | Container::Mp3) => TrackKind::Audio,
        Some(Container::Mp4 | Container::Ts | Container::Matroska | Container::Webm) => {
            TrackKind::Video
        }
        None => TrackKind::Unknown,
    }
}

/// A name for a track kind.
fn kind_label(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "Video",
        TrackKind::Audio => "Audio",
        TrackKind::Text => "Text",
        TrackKind::Unknown => "Media",
    }
}

/// A name for an audio rendition, preferring the one the manifest gives it.
fn rendition_label(rendition: &hls::Rendition) -> String {
    match (rendition.name(), rendition.language()) {
        (Some(name), Some(language)) => format!("{name} ({language})"),
        (Some(name), None) => name.to_owned(),
        (None, Some(language)) => format!("Audio ({language})"),
        (None, None) => "Audio".to_owned(),
    }
}
