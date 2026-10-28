//! Turning a parsed manifest into an ordered download plan.
//!
//! A plan is the sequence of resources that must be fetched and concatenated,
//! in order, to produce one output file: initialization segments interleaved
//! with media segments, exactly as a player would consume them.

use gunda_core::download::{ExecutionSelection, ResourceKind};
use url::Url;

use crate::MAX_SEGMENTS;
use crate::container::Container;
use crate::dash::{self, Mpd, MpdKind, RepresentationAddressing, TrackKind};
use crate::error::{ManifestError, UnsupportedFeature};
use crate::hls::{self, ByteRange, Playlist};
use crate::selection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Hls,
    Dash,
}

impl StreamKind {
    /// Returns the domain resource kind for this flavor.
    #[must_use]
    pub const fn resource_kind(self) -> ResourceKind {
        match self {
            Self::Hls => ResourceKind::Hls,
            Self::Dash => ResourceKind::Dash,
        }
    }
}

/// One resource of a plan: an absolute URL plus an optional byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamResource {
    url: Url,
    byte_range: Option<ByteRange>,
    track: u8,
}

impl StreamResource {
    #[must_use]
    pub fn new(url: Url, byte_range: Option<ByteRange>) -> Self {
        Self {
            url,
            byte_range,
            track: 0,
        }
    }

    /// Records which track of a presentation this resource carries.
    ///
    /// A presentation that splits video from audio is planned as one ordered
    /// list of resources, and the track is what tells the executor's muxer
    /// which file each resource belongs in.
    #[must_use]
    pub const fn with_track(mut self, track: u8) -> Self {
        self.track = track;
        self
    }

    #[must_use]
    pub const fn track(&self) -> u8 {
        self.track
    }

    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    #[must_use]
    pub const fn byte_range(&self) -> Option<ByteRange> {
        self.byte_range
    }

    /// Returns the number of bytes this resource is expected to contribute when
    /// that is knowable from the manifest alone.
    #[must_use]
    pub const fn known_length(&self) -> Option<u64> {
        match self.byte_range {
            Some(range) => Some(range.length()),
            None => None,
        }
    }

    /// Returns the inclusive byte interval this resource covers, if it is one
    /// interval of a larger resource.
    ///
    /// An `EXT-X-BYTERANGE` without an offset continues after the previous
    /// sub-range, which the playlist parser resolves while reading the
    /// playlist; an offset that is absent here therefore means the start of the
    /// resource.
    #[must_use]
    pub const fn byte_interval(&self) -> Option<(u64, u64)> {
        match self.byte_range {
            Some(range) => {
                let first = match range.offset() {
                    Some(offset) => offset,
                    None => 0,
                };

                Some((
                    first,
                    first.saturating_add(range.length().saturating_sub(1)),
                ))
            }
            None => None,
        }
    }
}

/// An executable download plan for one media track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPlan {
    kind: StreamKind,
    resources: Vec<StreamResource>,
    duration_millis: Option<u64>,
    label: Option<String>,
    bandwidth: Option<u64>,
    declared_total_bytes: Option<u64>,
    container_hint: Option<Container>,
}

impl TrackPlan {
    pub fn new(
        kind: StreamKind,
        resources: Vec<StreamResource>,
        duration_millis: Option<u64>,
        label: Option<String>,
    ) -> Result<Self, ManifestError> {
        if resources.is_empty() {
            return Err(ManifestError::Malformed);
        }

        if resources.len() > MAX_SEGMENTS {
            return Err(ManifestError::Malformed);
        }

        Ok(Self {
            kind,
            resources,
            duration_millis,
            label,
            bandwidth: None,
            declared_total_bytes: None,
            container_hint: None,
        })
    }

    /// Records the container the format itself implies.
    ///
    /// DASH media segments, and HLS segments that need an initialization
    /// segment, are ISO base media by construction; an HLS playlist without one
    /// carries transport-stream packets. Bytes read from the origin refine this
    /// and win when the two disagree.
    #[must_use]
    pub fn with_container_hint(mut self, hint: Container) -> Self {
        self.container_hint = Some(hint);

        self
    }

    #[must_use]
    pub fn with_bandwidth(mut self, bandwidth: Option<u64>) -> Self {
        self.bandwidth = bandwidth;
        self
    }

    #[must_use]
    pub fn with_declared_total(mut self, total: Option<u64>) -> Self {
        self.declared_total_bytes = total;
        self
    }

    /// The container the manifest implies, when it implies one.
    #[must_use]
    pub const fn container_hint(&self) -> Option<Container> {
        self.container_hint
    }

    #[must_use]
    pub const fn kind(&self) -> StreamKind {
        self.kind
    }

    #[must_use]
    pub fn resources(&self) -> &[StreamResource] {
        &self.resources
    }

    #[must_use]
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    #[must_use]
    pub const fn duration_millis(&self) -> Option<u64> {
        self.duration_millis
    }

    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// Returns the total byte length when every resource declares one.
    ///
    /// Segment sizes are normally unknown until they are fetched, so callers
    /// must treat `None` as "indeterminate", not as "empty".
    #[must_use]
    pub fn known_total_bytes(&self) -> Option<u64> {
        self.resources
            .iter()
            .map(StreamResource::known_length)
            .try_fold(0_u64, |total, length| {
                length.and_then(|length| total.checked_add(length))
            })
    }

    #[must_use]
    pub fn estimated_total_bytes(&self) -> Option<u64> {
        self.declared_total_bytes.or_else(|| {
            self.duration_millis
                .zip(self.bandwidth)
                .map(|(duration, bandwidth)| bandwidth.saturating_mul(duration) / 8000)
        })
    }

    /// How many tracks of the presentation the plan carries.
    ///
    /// One means the media is already whole and can be published as fetched.
    /// More than one means the tracks have to be muxed into a single file
    /// before it can be published at all.
    #[must_use]
    pub fn track_count(&self) -> usize {
        self.resources
            .iter()
            .map(StreamResource::track)
            .max()
            .map_or(0, |track| usize::from(track) + 1)
    }

    pub fn resources_of(&self, track: u8) -> impl Iterator<Item = &StreamResource> {
        self.resources
            .iter()
            .filter(move |resource| resource.track() == track)
    }

    /// An audio playlist is planned like the video one; only its staging file
    /// differs.
    #[must_use]
    pub fn retracked(mut self, track: u8) -> Self {
        for resource in &mut self.resources {
            *resource = resource.clone().with_track(track);
        }

        self
    }

    /// A presentation that keeps its audio in a track of its own can only
    /// become one file by being muxed. ISO base media tracks are carried over
    /// as they stand; a transport stream or an ADTS audio file is demuxed
    /// first. Anything else is refused rather than published without its
    /// sound.
    pub fn with_audio(mut self, audio: Self) -> Result<Self, ManifestError> {
        let muxable = |container: Option<Container>| container.is_some_and(Container::can_be_muxed);

        if !muxable(self.container_hint) || !muxable(audio.container_hint) {
            return Err(UnsupportedFeature::SeparateTracks.into());
        }

        if self.resources.len().saturating_add(audio.resources.len()) > MAX_SEGMENTS {
            return Err(ManifestError::Malformed);
        }

        self.resources.extend(
            audio
                .resources
                .into_iter()
                .map(|resource| resource.with_track(1)),
        );

        // The two tracks become one ISO base media file, and the download is as
        // long as its longest track.
        self.container_hint = Some(Container::Mp4);
        self.duration_millis = self.duration_millis.max(audio.duration_millis);
        self.bandwidth = match (self.bandwidth, audio.bandwidth) {
            (Some(video), Some(audio)) => Some(video.saturating_add(audio)),
            (Some(bandwidth), None) | (None, Some(bandwidth)) => Some(bandwidth),
            (None, None) => None,
        };
        self.declared_total_bytes = match (self.declared_total_bytes, audio.declared_total_bytes) {
            (Some(video), Some(audio)) => Some(video.saturating_add(audio)),
            (Some(total), None) | (None, Some(total)) => Some(total),
            (None, None) => None,
        };

        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsOutcome {
    FetchVariant {
        url: Url,
        label: Option<String>,
        bandwidth: Option<u64>,
        declared_total: Option<u64>,
        /// The audio rendition that belongs with the variant, when the variant
        /// carries no audio of its own.
        audio: Option<Url>,
    },
    Ready(TrackPlan),
}

/// Bandwidth is the only ordering signal a master playlist always provides, but
/// it is not the only one that matters when the sound lives in a rendition of
/// its own: the same picture is often offered with stereo AAC and with surround
/// Dolby, and the larger audio track gives its variant the higher bandwidth.
/// Only the AAC rendition can be combined into a single file today, so when a
/// master names more than one audio group the best variant that declares AAC
/// wins. Otherwise, or when no variant declares AAC, bandwidth decides.
fn select_variant(master: &hls::MasterPlaylist) -> Option<&hls::Variant> {
    let mut groups: Vec<&str> = master
        .variants()
        .iter()
        .filter_map(hls::Variant::audio_group)
        .collect();

    groups.sort_unstable();
    groups.dedup();

    if groups.len() > 1
        && let Some(variant) = master
            .variants()
            .iter()
            .filter(|variant| declares_aac(variant.codecs()))
            .max_by_key(|variant| {
                (
                    variant.bandwidth().unwrap_or(0),
                    variant.width().unwrap_or(0),
                    variant.height().unwrap_or(0),
                )
            })
    {
        return Some(variant);
    }

    master.best_variant()
}

fn declares_aac(codecs: Option<&str>) -> bool {
    codecs.is_some_and(|codecs| {
        codecs
            .split(',')
            .any(|codec| codec.trim().to_ascii_lowercase().starts_with("mp4a"))
    })
}

/// `base` must be the URL the playlist was fetched from, so relative segment
/// references resolve as a player would resolve them.
pub fn plan_hls(base: &Url, playlist: &Playlist) -> Result<HlsOutcome, ManifestError> {
    plan_hls_selected(base, playlist, None)
}

pub fn plan_hls_selected(
    base: &Url,
    playlist: &Playlist,
    selection: Option<&ExecutionSelection>,
) -> Result<HlsOutcome, ManifestError> {
    match playlist {
        Playlist::Master(master) => {
            let variant = match selection {
                Some(selection) => selected_variant(master, selection)?,
                None => select_variant(master).ok_or(ManifestError::Malformed)?,
            };

            let url = resolve(base, variant.uri(), None)?.url;

            // A variant that names an audio group whose renditions have URIs of
            // their own carries no audio itself, so the rendition is part of
            // the download rather than something to leave behind.
            let audio = match selected_audio_rendition(master, variant, selection)?
                .and_then(hls::Rendition::uri)
            {
                Some(uri) => Some(resolve(base, uri, None)?.url),
                None => None,
            };

            Ok(HlsOutcome::FetchVariant {
                url,
                label: label_for_variant(variant),
                bandwidth: variant.bandwidth(),
                declared_total: selection
                    .and_then(|selection| selection::parse_hls(selection).ok())
                    .and_then(|selection| selection.declared_total),
                audio,
            })
        }
        Playlist::Media(media) if selection.is_none() => {
            plan_media_playlist(base, media, 0).map(HlsOutcome::Ready)
        }
        Playlist::Media(_) => Err(ManifestError::SelectionUnavailable),
    }
}

fn selected_audio_rendition<'a>(
    master: &'a hls::MasterPlaylist,
    variant: &hls::Variant,
    selection: Option<&ExecutionSelection>,
) -> Result<Option<&'a hls::Rendition>, ManifestError> {
    let Some(selection) = selection else {
        return Ok(master.default_audio_rendition(variant));
    };
    let selected = selection::parse_hls(selection)?;
    let Some(index) = selected.audio_rendition else {
        return Ok(master.default_audio_rendition(variant));
    };
    let rendition = master
        .renditions()
        .get(index)
        .ok_or(ManifestError::SelectionUnavailable)?;
    if !rendition.kind().eq_ignore_ascii_case("AUDIO")
        || rendition.uri().is_none()
        || rendition.group_id() != variant.audio_group()
        || rendition.language() != selected.audio_language.as_deref()
    {
        return Err(ManifestError::SelectionUnavailable);
    }

    Ok(Some(rendition))
}

fn selected_variant<'a>(
    master: &'a hls::MasterPlaylist,
    selection: &ExecutionSelection,
) -> Result<&'a hls::Variant, ManifestError> {
    let selected = selection::parse_hls(selection)?;
    let variant = master
        .variants()
        .get(selected.variant)
        .ok_or(ManifestError::SelectionUnavailable)?;

    if variant.bandwidth() != selected.bandwidth || variant.resolution() != selected.resolution {
        return Err(ManifestError::SelectionUnavailable);
    }

    Ok(variant)
}

fn plan_media_playlist(
    base: &Url,
    playlist: &hls::MediaPlaylist,
    track: u8,
) -> Result<TrackPlan, ManifestError> {
    if playlist.is_encrypted() {
        return Err(UnsupportedFeature::EncryptedSegments.into());
    }

    // A playlist without `EXT-X-ENDLIST` is still growing; capturing it now
    // would silently publish a truncated file.
    if !playlist.is_ended() {
        return Err(UnsupportedFeature::IncompletePresentation.into());
    }

    if playlist.segments().len().saturating_add(1) > MAX_SEGMENTS {
        return Err(ManifestError::TooManySegments);
    }

    let mut resources = Vec::new();
    let mut current_init: Option<hls::ResourceRef> = None;

    for segment in playlist.segments() {
        match segment.init() {
            Some(init) => {
                if current_init.as_ref() != Some(init) {
                    resources.push(resolve(base, init.uri(), init.byte_range())?.with_track(track));
                    current_init = Some(init.clone());
                }
            }
            None => current_init = None,
        }

        resources.push(
            resolve(
                base,
                segment.resource().uri(),
                segment.resource().byte_range(),
            )?
            .with_track(track),
        );
    }

    // `EXT-X-MAP` exists to carry an ISO base media initialization segment, so
    // a playlist that uses one is fragmented MP4; one that does not is a
    // transport stream, or packed audio that the bytes themselves identify.
    let hint = if playlist
        .segments()
        .iter()
        .any(|segment| segment.init().is_some())
    {
        Container::Mp4
    } else {
        Container::Ts
    };

    Ok(TrackPlan::new(
        StreamKind::Hls,
        resources,
        playlist.total_duration_millis(),
        None,
    )?
    .with_container_hint(hint))
}

pub(crate) fn label_for_variant(variant: &hls::Variant) -> Option<String> {
    match (variant.resolution(), variant.bandwidth()) {
        (Some((width, height)), Some(bandwidth)) => {
            Some(format!("{width}x{height} ({})", bandwidth_hint(bandwidth)))
        }
        (Some((width, height)), None) => Some(format!("{width}x{height}")),
        (None, Some(bandwidth)) => Some(bandwidth_hint(bandwidth)),
        (None, None) => None,
    }
}

pub(crate) fn bandwidth_hint(bandwidth: u64) -> String {
    if bandwidth >= 1_000_000 {
        format!("{:.1} Mbps", bandwidth as f64 / 1_000_000.0)
    } else {
        format!("{} kbps", bandwidth / 1000)
    }
}

struct PlannedSet {
    resources: Vec<StreamResource>,
    label: Option<String>,
    container: Container,
    duration_millis: Option<u64>,
}

/// A presentation that splits video from audio is planned as both tracks. They
/// cannot be concatenated into one file, so they are fetched side by side and
/// muxed; leaving the audio out would publish a silently silent video.
pub fn plan_dash(base: &Url, mpd: &Mpd) -> Result<TrackPlan, ManifestError> {
    plan_dash_selected(base, mpd, None)
}

pub fn plan_dash_selected(
    base: &Url,
    mpd: &Mpd,
    selection: Option<&ExecutionSelection>,
) -> Result<TrackPlan, ManifestError> {
    if mpd.kind() == MpdKind::Dynamic {
        return Err(UnsupportedFeature::IncompletePresentation.into());
    }

    let period = mpd.media_period()?;
    let tracks = match selection {
        Some(selection) => selected_dash_tracks(period, selection)?,
        None => default_dash_tracks(period)?,
    };

    let video_plan = tracks
        .video
        .map(|(set, representation)| plan_dash_track(base, mpd, period, set, representation))
        .transpose()?;
    let audio_plan = tracks
        .audio
        .map(|(set, representation)| plan_dash_track(base, mpd, period, set, representation))
        .transpose()?;

    match (video_plan, audio_plan) {
        (Some(video), Some(audio)) => video.with_audio(audio),
        (Some(video), None) => Ok(video),
        (None, Some(audio)) => Ok(audio),
        (None, None) => Err(ManifestError::Malformed),
    }
}

struct DashTracks {
    video: Option<(usize, usize)>,
    audio: Option<(usize, usize)>,
}

fn default_dash_tracks(period: &dash::Period) -> Result<DashTracks, ManifestError> {
    let video_set = select_set_index(period).ok_or(ManifestError::Malformed)?;
    let representation = best_representation_index(&period.adaptation_sets()[video_set])
        .ok_or(ManifestError::Malformed)?;
    let video = (video_set, representation);
    let audio = if period.adaptation_sets()[video_set].kind() == TrackKind::Video {
        audio_set_index(period).and_then(|set| {
            best_representation_index(&period.adaptation_sets()[set]).map(|rep| (set, rep))
        })
    } else {
        None
    };

    Ok(DashTracks {
        video: Some(video),
        audio,
    })
}

fn selected_dash_tracks(
    period: &dash::Period,
    selection: &ExecutionSelection,
) -> Result<DashTracks, ManifestError> {
    let selected = selection::parse_dash(selection)?;

    let video = match (selected.video_set, selected.video_representation) {
        (Some(set), Some(representation)) => {
            validate_dash_choice(
                period,
                set,
                representation,
                TrackKind::Video,
                selected.video_bandwidth,
            )?;
            Some((set, representation))
        }
        (None, None) => None,
        _ => return Err(ManifestError::SelectionUnavailable),
    };
    let audio = match (selected.audio_set, selected.audio_representation) {
        (Some(set), Some(representation)) => {
            validate_dash_choice(
                period,
                set,
                representation,
                TrackKind::Audio,
                selected.audio_bandwidth,
            )?;
            Some((set, representation))
        }
        (None, None) => None,
        _ => return Err(ManifestError::SelectionUnavailable),
    };

    Ok(DashTracks { video, audio })
}

fn validate_dash_choice(
    period: &dash::Period,
    set_index: usize,
    representation_index: usize,
    kind: TrackKind,
    bandwidth: Option<u64>,
) -> Result<(), ManifestError> {
    let set = period
        .adaptation_sets()
        .get(set_index)
        .ok_or(ManifestError::SelectionUnavailable)?;

    if set.kind() != kind {
        return Err(ManifestError::SelectionUnavailable);
    }

    let representation = set
        .representations()
        .get(representation_index)
        .ok_or(ManifestError::SelectionUnavailable)?;

    if representation.bandwidth() != bandwidth {
        return Err(ManifestError::SelectionUnavailable);
    }

    Ok(())
}

fn plan_dash_track(
    base: &Url,
    mpd: &Mpd,
    period: &dash::Period,
    set_index: usize,
    representation_index: usize,
) -> Result<TrackPlan, ManifestError> {
    let set = period
        .adaptation_sets()
        .get(set_index)
        .ok_or(ManifestError::SelectionUnavailable)?;

    let PlannedSet {
        resources,
        label,
        container,
        duration_millis,
    } = plan_set(base, mpd, period, set, representation_index)?;
    let bandwidth = set
        .representations()
        .get(representation_index)
        .and_then(dash::Representation::bandwidth);

    TrackPlan::new(StreamKind::Dash, resources, duration_millis, label).map(|plan| {
        plan.with_container_hint(container)
            .with_bandwidth(bandwidth)
    })
}

fn plan_set(
    base: &Url,
    mpd: &Mpd,
    period: &dash::Period,
    set: &dash::AdaptationSet,
    representation_index: usize,
) -> Result<PlannedSet, ManifestError> {
    let representation = set
        .representations()
        .get(representation_index)
        .ok_or(ManifestError::SelectionUnavailable)?;

    let mut resolved = base.clone();

    for chain in [
        mpd.base_urls(),
        period.base_urls(),
        set.base_urls(),
        representation.base_urls(),
    ] {
        if let Some(first) = chain.first() {
            resolved = resolved.join(first).map_err(|_| ManifestError::Malformed)?;
        }
    }

    let representation_id = representation.id();
    let bandwidth = representation.bandwidth();
    let period_end_millis = period.duration_millis().or_else(|| mpd.duration_millis());

    let resources = match representation.addressing(set, period) {
        RepresentationAddressing::Template(template) => {
            let media = template.media().ok_or(ManifestError::Malformed)?;
            let timescale = template.timescale();
            let mut resources = Vec::new();

            if let Some(initialization) = template.initialization() {
                let uri =
                    substitute_template(initialization, representation_id, bandwidth, None, None)?;

                resources.push(resolve(&resolved, &uri, None)?);
            }

            if let Some(timeline) = template.timeline() {
                let period_end =
                    period_end_millis.map(|millis| millis.saturating_mul(timescale) / 1000);
                let times = dash::expand_timeline(timeline, period_end)?;

                if resources.len().saturating_add(times.len()) > MAX_SEGMENTS {
                    return Err(ManifestError::TooManySegments);
                }

                for time in times {
                    let uri =
                        substitute_template(media, representation_id, bandwidth, None, Some(time))?;
                    resources.push(resolve(&resolved, &uri, None)?);
                }
            } else {
                let duration = template.duration().ok_or(ManifestError::Malformed)?;
                let count = segment_count(period_end_millis, timescale, duration)?;
                let available = MAX_SEGMENTS.saturating_sub(resources.len());
                if count > u64::try_from(available).unwrap_or(u64::MAX) {
                    return Err(ManifestError::TooManySegments);
                }
                let start_number = template.start_number();

                for index in 0..count {
                    let number = start_number.saturating_add(index);
                    let uri = substitute_template(
                        media,
                        representation_id,
                        bandwidth,
                        Some(number),
                        None,
                    )?;
                    resources.push(resolve(&resolved, &uri, None)?);
                }
            }

            resources
        }
        RepresentationAddressing::List(list) => {
            let mut resources = Vec::new();

            if let Some(initialization) = list.initialization() {
                let byte_range = initialization.range().map(|(first, last)| {
                    ByteRange::new(last.saturating_sub(first).saturating_add(1), Some(first))
                });

                resources.push(resolve(&resolved, initialization.source_url(), byte_range)?);
            }

            let urls = list.segment_urls().ok_or(ManifestError::Malformed)?;
            if resources.len().saturating_add(urls.len()) > MAX_SEGMENTS {
                return Err(ManifestError::TooManySegments);
            }

            for segment in urls {
                let byte_range = segment.media_range().map(|(first, last)| {
                    ByteRange::new(last.saturating_sub(first).saturating_add(1), Some(first))
                });

                resources.push(resolve(
                    &resolved,
                    segment.media().unwrap_or_default(),
                    byte_range,
                )?);
            }

            resources
        }
        // Enumerating a `SegmentBase` requires reading the segment index, which
        // is a separate mechanism this engine does not implement.
        RepresentationAddressing::Base(_) => {
            return Err(UnsupportedFeature::SegmentIndex.into());
        }
        RepresentationAddressing::SingleFile => {
            if resolved.path().is_empty() || resolved.path().ends_with('/') {
                return Err(UnsupportedFeature::SegmentIndex.into());
            }

            vec![StreamResource::new(resolved.clone(), None)]
        }
    };

    let label = match (representation.resolution(), representation.id()) {
        (Some((width, height)), Some(id)) => Some(format!("{width}x{height} ({id})")),
        (Some((width, height)), None) => Some(format!("{width}x{height}")),
        (None, Some(id)) => Some(id.to_owned()),
        (None, None) => None,
    };

    // DASH media segments are ISO base media; the track kind decides whether
    // the result is a video file or an audio-only one.
    let hint = if set.kind() == TrackKind::Audio {
        Container::M4a
    } else {
        Container::Mp4
    };

    Ok(PlannedSet {
        resources,
        label,
        container: hint,
        duration_millis: period_end_millis,
    })
}

/// Index of the audio adaptation set a player would pick by default.
pub(crate) fn audio_set_index(period: &dash::Period) -> Option<usize> {
    period
        .adaptation_sets()
        .iter()
        .enumerate()
        .filter(|(_, set)| set.kind() == TrackKind::Audio && !set.representations().is_empty())
        .max_by_key(|(_, set)| best_bandwidth(set))
        .map(|(index, _)| index)
}

/// Index of the adaptation set a download uses when no quality was chosen.
pub(crate) fn select_set_index(period: &dash::Period) -> Option<usize> {
    let populated = || {
        period
            .adaptation_sets()
            .iter()
            .enumerate()
            .filter(|(_, set)| !set.representations().is_empty())
    };

    populated()
        .filter(|(_, set)| set.kind() == TrackKind::Video)
        .max_by_key(|(_, set)| best_bandwidth(set))
        .or_else(|| {
            populated()
                .filter(|(_, set)| set.kind() != TrackKind::Text)
                .max_by_key(|(_, set)| best_bandwidth(set))
        })
        .map(|(index, _)| index)
}

pub(crate) fn best_representation_index(set: &dash::AdaptationSet) -> Option<usize> {
    set.representations()
        .iter()
        .enumerate()
        .max_by_key(|(_, representation)| {
            (
                representation.bandwidth().unwrap_or(0),
                representation.resolution().map_or(0, |(width, _)| width),
                representation.resolution().map_or(0, |(_, height)| height),
            )
        })
        .map(|(index, _)| index)
}

fn best_bandwidth(set: &dash::AdaptationSet) -> u64 {
    set.representations()
        .iter()
        .map(|representation| representation.bandwidth().unwrap_or(0))
        .max()
        .unwrap_or(0)
}

fn segment_count(
    duration_millis: Option<u64>,
    timescale: u64,
    duration: u64,
) -> Result<u64, ManifestError> {
    let duration_millis = duration_millis.ok_or(UnsupportedFeature::IncompletePresentation)?;

    if duration == 0 || timescale == 0 {
        return Err(ManifestError::Malformed);
    }

    let ticks = u128::from(duration_millis) * u128::from(timescale);
    let per_segment = u128::from(duration) * 1000;

    u64::try_from(ticks.div_ceil(per_segment)).map_err(|_| ManifestError::InvalidNumber)
}

/// Expands a DASH template such as `$RepresentationID$/seg-$Number%05d$.m4s`.
///
/// An identifier that is recognized but has no value, or that is not
/// recognized at all, is an error: fetching a URL with an unsubstituted `$`
/// would silently download the wrong resource.
pub fn substitute_template(
    template: &str,
    representation_id: Option<&str>,
    bandwidth: Option<u64>,
    number: Option<u64>,
    time: Option<u64>,
) -> Result<String, ManifestError> {
    if !template.contains('$') {
        return Ok(template.to_owned());
    }

    let mut expanded = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find('$') {
        expanded.push_str(&rest[..start]);

        let after = &rest[start + 1..];
        let end = after.find('$').ok_or(ManifestError::Malformed)?;
        let token = &after[..end];
        rest = &after[end + 1..];

        if token.is_empty() {
            // `$$` is an escaped literal dollar sign.
            expanded.push('$');
            continue;
        }

        let (identifier, format) = match token.split_once('%') {
            Some((identifier, format)) => (identifier, Some(format)),
            None => (token, None),
        };

        match identifier {
            "RepresentationID" => {
                if format.is_some() {
                    return Err(ManifestError::Malformed);
                }

                expanded.push_str(representation_id.ok_or(ManifestError::Malformed)?);
            }
            "Bandwidth" => {
                expanded.push_str(&format_number(
                    bandwidth.ok_or(ManifestError::Malformed)?,
                    format,
                )?);
            }
            "Number" => {
                expanded.push_str(&format_number(
                    number.ok_or(ManifestError::Malformed)?,
                    format,
                )?);
            }
            "Time" => {
                expanded.push_str(&format_number(
                    time.ok_or(ManifestError::Malformed)?,
                    format,
                )?);
            }
            _ => return Err(ManifestError::Malformed),
        }
    }

    expanded.push_str(rest);

    Ok(expanded)
}

fn format_number(value: u64, format: Option<&str>) -> Result<String, ManifestError> {
    let Some(format) = format else {
        return Ok(value.to_string());
    };

    let width = format
        .strip_suffix('d')
        .unwrap_or(format)
        .trim_start_matches('0');

    if width.is_empty() {
        return Ok(value.to_string());
    }

    let width = width
        .parse::<usize>()
        .map_err(|_| ManifestError::Malformed)?;

    if width == 0 || width > 32 {
        return Err(ManifestError::Malformed);
    }

    Ok(format!("{value:0width$}"))
}

fn resolve(
    base: &Url,
    uri: &str,
    byte_range: Option<ByteRange>,
) -> Result<StreamResource, ManifestError> {
    let url = base.join(uri).map_err(|_| ManifestError::Malformed)?;

    // Only fetchable schemes are accepted, so a manifest cannot steer the
    // engine at a local file or a custom protocol handler.
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ManifestError::Malformed);
    }

    Ok(StreamResource::new(url, byte_range))
}

#[cfg(test)]
mod tests {
    use gunda_core::download::ExecutionSelection;
    use url::Url;

    use super::{
        Container, HlsOutcome, StreamKind, StreamResource, TrackPlan, plan_dash,
        plan_dash_selected, plan_hls, plan_hls_selected, substitute_template,
    };

    #[test]
    fn estimates_size_from_declared_bandwidth_and_duration() {
        let plan = TrackPlan::new(
            StreamKind::Hls,
            vec![StreamResource::new(
                url("https://cdn.example/segment.ts"),
                None,
            )],
            Some(10_000),
            None,
        )
        .unwrap()
        .with_bandwidth(Some(800_000));

        assert_eq!(plan.estimated_total_bytes(), Some(1_000_000));
    }
    use crate::dash;
    use crate::error::{ManifestError, UnsupportedFeature};
    use crate::hls;

    fn url(text: &str) -> Url {
        Url::parse(text).expect("test URL must parse")
    }

    #[test]
    fn master_playlist_asks_for_the_best_variant() {
        let playlist = hls::parse(
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n\
             low.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1920x1080\n\
             /vod/high.m3u8\n",
        )
        .expect("playlist must parse");

        let outcome = plan_hls(&url("https://cdn.example/vod/master.m3u8"), &playlist)
            .expect("planning must succeed");

        match outcome {
            HlsOutcome::FetchVariant {
                url, label, audio, ..
            } => {
                assert_eq!(url.as_str(), "https://cdn.example/vod/high.m3u8");
                assert_eq!(label.as_deref(), Some("1920x1080 (2.4 Mbps)"));
                assert!(audio.is_none());
            }
            HlsOutcome::Ready(_) => panic!("expected a variant fetch"),
        }
    }

    #[test]
    fn a_selected_hls_variant_is_planned_with_its_paired_audio() {
        let playlist = hls::parse(
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,AUDIO=\"aac\"\n\
             low.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1920x1080,AUDIO=\"aac\"\n\
             high.m3u8\n",
        )
        .expect("playlist must parse");
        let selection = ExecutionSelection::new("hls:1:0:800000:640:360").unwrap();

        let outcome = plan_hls_selected(
            &url("https://cdn.example/vod/master.m3u8"),
            &playlist,
            Some(&selection),
        )
        .expect("planning must succeed");

        match outcome {
            HlsOutcome::FetchVariant { url, audio, .. } => {
                assert_eq!(url.as_str(), "https://cdn.example/vod/low.m3u8");
                assert_eq!(
                    audio
                        .expect("the paired rendition is part of the download")
                        .as_str(),
                    "https://cdn.example/vod/audio.m3u8"
                );
            }
            HlsOutcome::Ready(_) => panic!("expected a variant fetch"),
        }
    }

    #[test]
    fn a_selected_hls_variant_uses_the_requested_audio_language() {
        let playlist = hls::parse(
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"en.m3u8\"\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"Portuguese\",LANGUAGE=\"pt\",URI=\"pt.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,AUDIO=\"aac\"\n\
             low.m3u8\n",
        )
        .expect("playlist must parse");
        let selection = ExecutionSelection::new("hls:2:0:800000:640:360:1:pt").unwrap();

        let outcome = plan_hls_selected(
            &url("https://cdn.example/vod/master.m3u8"),
            &playlist,
            Some(&selection),
        )
        .expect("planning must succeed");

        let HlsOutcome::FetchVariant { audio, .. } = outcome else {
            panic!("expected a variant fetch");
        };
        assert_eq!(
            audio.expect("the requested rendition is selected").as_str(),
            "https://cdn.example/vod/pt.m3u8"
        );
    }

    #[test]
    fn a_selection_whose_variant_changed_is_refused() {
        let playlist = hls::parse(
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n\
             low.m3u8\n",
        )
        .expect("playlist must parse");
        let selection = ExecutionSelection::new("hls:1:0:2400000:640:360").unwrap();

        assert!(matches!(
            plan_hls_selected(
                &url("https://cdn.example/vod/master.m3u8"),
                &playlist,
                Some(&selection)
            ),
            Err(ManifestError::SelectionUnavailable)
        ));
    }

    #[test]
    fn a_selected_dash_representation_is_planned_with_the_chosen_audio() {
        let mpd = dash::parse(
            r#"<MPD type="static" mediaPresentationDuration="PT4S">
                 <Period>
                   <AdaptationSet contentType="video" mimeType="video/mp4">
                     <SegmentTemplate timescale="1000" duration="4000"
                                      initialization="$RepresentationID$/init.mp4"
                                      media="$RepresentationID$/$Number$.m4s"/>
                     <Representation id="v0" bandwidth="800000" width="640" height="360"/>
                     <Representation id="v1" bandwidth="2400000" width="1920" height="1080"/>
                   </AdaptationSet>
                   <AdaptationSet contentType="audio" mimeType="audio/mp4" lang="en">
                     <SegmentTemplate timescale="1000" duration="4000"
                                      initialization="$RepresentationID$/init.mp4"
                                      media="$RepresentationID$/$Number$.m4s"/>
                     <Representation id="a0" bandwidth="128000"/>
                   </AdaptationSet>
                 </Period>
               </MPD>"#,
        )
        .expect("manifest must parse");
        let selection = ExecutionSelection::new("dash:1:0:0:800000:1:0:128000").unwrap();

        let plan = plan_dash_selected(
            &url("https://cdn.example/dash/manifest.mpd"),
            &mpd,
            Some(&selection),
        )
        .expect("planning must succeed");

        let urls: Vec<&str> = plan
            .resources()
            .iter()
            .map(|resource| resource.url().as_str())
            .collect();

        assert!(urls.contains(&"https://cdn.example/dash/v0/init.mp4"));
        assert!(urls.contains(&"https://cdn.example/dash/v0/1.m4s"));
        assert!(urls.contains(&"https://cdn.example/dash/a0/init.mp4"));
        assert!(!urls.iter().any(|url| url.contains("/v1/")));
        assert_eq!(plan.track_count(), 2);
    }

    #[test]
    fn a_master_with_several_audio_groups_prefers_the_one_this_can_carry() {
        let playlist = hls::parse(
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"stereo\",NAME=\"English\",DEFAULT=YES,URI=\"a1/index.m3u8\"\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"surround\",NAME=\"English\",DEFAULT=YES,CHANNELS=\"6\",URI=\"a2/index.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080,CODECS=\"avc1.64002a,mp4a.40.2\",AUDIO=\"stereo\"\n\
             v9/index.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=8300000,RESOLUTION=1920x1080,CODECS=\"avc1.64002a,ac-3\",AUDIO=\"surround\"\n\
             v9-surround/index.m3u8\n",
        )
        .expect("playlist must parse");

        let outcome = plan_hls(&url("https://cdn.example/master.m3u8"), &playlist)
            .expect("planning must succeed");

        match outcome {
            HlsOutcome::FetchVariant { url, audio, .. } => {
                assert_eq!(url.as_str(), "https://cdn.example/v9/index.m3u8");
                assert_eq!(
                    audio.as_ref().map(Url::as_str),
                    Some("https://cdn.example/a1/index.m3u8")
                );
            }
            HlsOutcome::Ready(_) => panic!("expected a variant fetch"),
        }
    }

    #[test]
    fn a_single_audio_group_leaves_bandwidth_in_charge() {
        let playlist = hls::parse(
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"surround\",NAME=\"English\",DEFAULT=YES,URI=\"a2/index.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080,CODECS=\"avc1.64002a,ac-3\",AUDIO=\"surround\"\n\
             v9/index.m3u8\n",
        )
        .expect("playlist must parse");

        let outcome = plan_hls(&url("https://cdn.example/master.m3u8"), &playlist)
            .expect("planning must succeed");

        let HlsOutcome::FetchVariant { url, .. } = outcome else {
            panic!("expected a variant fetch");
        };

        assert_eq!(url.as_str(), "https://cdn.example/v9/index.m3u8");
    }

    #[test]
    fn media_playlist_resolves_absolute_urls_and_deduplicates_the_init_segment() {
        let playlist = hls::parse(
            "#EXTM3U\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:4,\n\
             seg1.m4s\n\
             #EXTINF:4,\n\
             seg2.m4s\n\
             #EXT-X-ENDLIST\n",
        )
        .expect("playlist must parse");

        let outcome = plan_hls(&url("https://cdn.example/vod/media.m3u8"), &playlist)
            .expect("planning must succeed");

        let HlsOutcome::Ready(plan) = outcome else {
            panic!("expected a ready plan");
        };

        let urls: Vec<&str> = plan
            .resources()
            .iter()
            .map(|resource| resource.url().as_str())
            .collect();

        assert_eq!(
            urls,
            [
                "https://cdn.example/vod/init.mp4",
                "https://cdn.example/vod/seg1.m4s",
                "https://cdn.example/vod/seg2.m4s",
            ]
        );
        assert_eq!(plan.kind(), StreamKind::Hls);
        assert_eq!(plan.duration_millis(), Some(8_000));
        assert_eq!(plan.known_total_bytes(), None);
    }

    #[test]
    fn encrypted_playlists_are_refused() {
        let playlist = hls::parse(
            "#EXTM3U\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
             #EXTINF:4,\n\
             seg1.ts\n\
             #EXT-X-ENDLIST\n",
        )
        .expect("playlist must parse");

        assert!(matches!(
            plan_hls(&url("https://cdn.example/media.m3u8"), &playlist),
            Err(ManifestError::Unsupported(
                UnsupportedFeature::EncryptedSegments
            ))
        ));
    }

    #[test]
    fn live_playlists_are_refused() {
        let playlist = hls::parse("#EXTM3U\n#EXTINF:4,\nseg1.ts\n").expect("playlist must parse");

        assert!(matches!(
            plan_hls(&url("https://cdn.example/media.m3u8"), &playlist),
            Err(ManifestError::Unsupported(
                UnsupportedFeature::IncompletePresentation
            ))
        ));
    }

    #[test]
    fn dash_number_templates_expand_every_segment() {
        let mpd = dash::parse(
            r#"<MPD type="static" mediaPresentationDuration="PT10S">
                 <BaseURL>https://cdn.example/dash/</BaseURL>
                 <Period>
                   <AdaptationSet contentType="video" mimeType="video/mp4">
                     <SegmentTemplate timescale="1000" duration="4000" startNumber="1"
                                      initialization="$RepresentationID$/init.mp4"
                                      media="$RepresentationID$/$Number%03d$.m4s"/>
                     <Representation id="v0" bandwidth="800000" width="1280" height="720"/>
                   </AdaptationSet>
                 </Period>
               </MPD>"#,
        )
        .expect("manifest must parse");

        let plan = plan_dash(&url("https://cdn.example/dash/manifest.mpd"), &mpd)
            .expect("planning must succeed");

        let urls: Vec<&str> = plan
            .resources()
            .iter()
            .map(|resource| resource.url().as_str())
            .collect();

        assert_eq!(
            urls,
            [
                "https://cdn.example/dash/v0/init.mp4",
                "https://cdn.example/dash/v0/001.m4s",
                "https://cdn.example/dash/v0/002.m4s",
                "https://cdn.example/dash/v0/003.m4s",
            ]
        );
        assert_eq!(plan.kind(), StreamKind::Dash);
        assert_eq!(plan.duration_millis(), Some(10_000));
        assert_eq!(plan.label(), Some("1280x720 (v0)"));
    }

    #[test]
    fn dash_template_segment_count_is_checked_before_expansion() {
        let mpd = dash::parse(
            r#"<MPD type="static" mediaPresentationDuration="PT1000000S">
                 <Period>
                   <AdaptationSet contentType="video" mimeType="video/mp4">
                     <SegmentTemplate timescale="1" duration="1"
                                      media="$RepresentationID$/$Number$.m4s"/>
                     <Representation id="v0" bandwidth="1"/>
                   </AdaptationSet>
                 </Period>
               </MPD>"#,
        )
        .expect("manifest must parse");

        assert!(matches!(
            plan_dash(&url("https://cdn.example/dash/manifest.mpd"), &mpd),
            Err(ManifestError::TooManySegments)
        ));
    }

    #[test]
    fn dash_timelines_use_absolute_times() {
        let mpd = dash::parse(
            r#"<MPD type="static">
                 <Period>
                   <AdaptationSet contentType="video" mimeType="video/mp4">
                     <SegmentTemplate timescale="1000" media="t-$Time$.m4s">
                       <SegmentTimeline>
                         <S t="0" d="2000" r="1"/>
                         <S d="1000"/>
                       </SegmentTimeline>
                     </SegmentTemplate>
                     <Representation id="v0" bandwidth="1"/>
                   </AdaptationSet>
                 </Period>
               </MPD>"#,
        )
        .expect("manifest must parse");

        let plan = plan_dash(&url("https://cdn.example/dash/manifest.mpd"), &mpd)
            .expect("planning must succeed");

        let urls: Vec<&str> = plan
            .resources()
            .iter()
            .map(|resource| resource.url().as_str())
            .collect();

        assert_eq!(
            urls,
            [
                "https://cdn.example/dash/t-0.m4s",
                "https://cdn.example/dash/t-2000.m4s",
                "https://cdn.example/dash/t-4000.m4s",
            ]
        );
    }

    #[test]
    fn dash_segment_lists_carry_byte_ranges() {
        let mpd = dash::parse(
            r#"<MPD type="static">
                 <Period>
                   <AdaptationSet contentType="video" mimeType="video/mp4">
                     <Representation id="v0" bandwidth="1">
                       <BaseURL>movie.mp4</BaseURL>
                       <SegmentList>
                         <Initialization sourceURL="movie.mp4" range="0-799"/>
                         <SegmentURL media="movie.mp4" mediaRange="800-4799"/>
                       </SegmentList>
                     </Representation>
                   </AdaptationSet>
                 </Period>
               </MPD>"#,
        )
        .expect("manifest must parse");

        let plan = plan_dash(&url("https://cdn.example/dash/manifest.mpd"), &mpd)
            .expect("planning must succeed");

        assert_eq!(plan.resource_count(), 2);

        let initialization = &plan.resources()[0];
        assert_eq!(
            initialization.url().as_str(),
            "https://cdn.example/dash/movie.mp4"
        );
        assert_eq!(
            initialization.byte_range().map(hls::ByteRange::length),
            Some(800)
        );

        let segment = &plan.resources()[1];
        assert_eq!(segment.url().as_str(), "https://cdn.example/dash/movie.mp4");
        assert_eq!(segment.byte_range().map(hls::ByteRange::length), Some(4000));
        assert_eq!(
            segment.byte_range().and_then(hls::ByteRange::offset),
            Some(800)
        );
        assert_eq!(plan.known_total_bytes(), Some(4800));
    }

    #[test]
    fn dash_single_file_representations_plan_one_resource() {
        let mpd = dash::parse(
            r#"<MPD type="static">
                 <Period>
                   <AdaptationSet contentType="audio" mimeType="audio/mp4">
                     <Representation id="a0" bandwidth="1">
                       <BaseURL>audio.mp4</BaseURL>
                     </Representation>
                   </AdaptationSet>
                 </Period>
               </MPD>"#,
        )
        .expect("manifest must parse");

        let plan = plan_dash(&url("https://cdn.example/dash/manifest.mpd"), &mpd)
            .expect("planning must succeed");

        assert_eq!(plan.resource_count(), 1);
        assert_eq!(
            plan.resources()[0].url().as_str(),
            "https://cdn.example/dash/audio.mp4"
        );
    }

    #[test]
    fn dash_separate_audio_and_video_are_planned_as_two_tracks() {
        let mpd = dash::parse(
            r#"<MPD type="static" mediaPresentationDuration="PT8S">
                 <Period>
                   <AdaptationSet contentType="video" mimeType="video/mp4">
                     <SegmentTemplate media="v-$Number$.m4s" duration="4000" timescale="1000"/>
                     <Representation id="v0" bandwidth="800000"/>
                   </AdaptationSet>
                   <AdaptationSet contentType="audio" mimeType="audio/mp4">
                     <SegmentTemplate media="a-$Number$.m4s" duration="4000" timescale="1000"/>
                     <Representation id="a0" bandwidth="128000"/>
                   </AdaptationSet>
                 </Period>
               </MPD>"#,
        )
        .expect("manifest must parse");

        let plan = plan_dash(&url("https://cdn.example/dash/manifest.mpd"), &mpd)
            .expect("planning must succeed");

        assert_eq!(plan.track_count(), 2);
        assert_eq!(plan.container_hint(), Some(Container::Mp4));

        let video: Vec<&str> = plan
            .resources_of(0)
            .map(|resource| resource.url().as_str())
            .collect();
        let audio: Vec<&str> = plan
            .resources_of(1)
            .map(|resource| resource.url().as_str())
            .collect();

        assert_eq!(
            video,
            [
                "https://cdn.example/dash/v-1.m4s",
                "https://cdn.example/dash/v-2.m4s"
            ]
        );
        assert_eq!(
            audio,
            [
                "https://cdn.example/dash/a-1.m4s",
                "https://cdn.example/dash/a-2.m4s"
            ]
        );

        // The sound is never left behind, and never merged into the picture's
        // resources, which a muxer could not then tell apart.
        assert_eq!(plan.resource_count(), 4);
    }

    #[test]
    fn dash_dynamic_presentations_are_refused() {
        let mpd = dash::parse(
            r#"<MPD type="dynamic">
                 <Period>
                   <AdaptationSet contentType="video" mimeType="video/mp4">
                     <SegmentTemplate media="$Number$.m4s" duration="4000" timescale="1000"/>
                     <Representation id="v0" bandwidth="1"/>
                   </AdaptationSet>
                 </Period>
               </MPD>"#,
        )
        .expect("manifest must parse");

        assert!(matches!(
            plan_dash(&url("https://cdn.example/dash/manifest.mpd"), &mpd),
            Err(ManifestError::Unsupported(
                UnsupportedFeature::IncompletePresentation
            ))
        ));
    }

    #[test]
    fn dash_segment_base_needs_an_index() {
        let mpd = dash::parse(
            r#"<MPD type="static">
                 <Period>
                   <AdaptationSet contentType="video" mimeType="video/mp4">
                     <Representation id="v0" bandwidth="1">
                       <BaseURL>movie.mp4</BaseURL>
                       <SegmentBase indexRange="800-1199"/>
                     </Representation>
                   </AdaptationSet>
                 </Period>
               </MPD>"#,
        )
        .expect("manifest must parse");

        assert!(matches!(
            plan_dash(&url("https://cdn.example/dash/manifest.mpd"), &mpd),
            Err(ManifestError::Unsupported(UnsupportedFeature::SegmentIndex))
        ));
    }

    #[test]
    fn non_http_references_are_refused() {
        let playlist = hls::parse("#EXTM3U\n#EXTINF:4,\nfile:///etc/passwd\n#EXT-X-ENDLIST\n")
            .expect("playlist must parse");

        assert!(plan_hls(&url("https://cdn.example/media.m3u8"), &playlist).is_err());

        let playlist = hls::parse("#EXTM3U\n#EXTINF:4,\nseg.ts\n#EXT-X-ENDLIST\n")
            .expect("playlist must parse");
        let plan = plan_hls(&url("https://cdn.example/a/b/media.m3u8"), &playlist)
            .expect("planning must succeed");

        let HlsOutcome::Ready(plan) = plan else {
            panic!("expected a ready plan");
        };

        assert_eq!(
            plan.resources()[0].url().as_str(),
            "https://cdn.example/a/b/seg.ts"
        );
    }

    #[test]
    fn template_substitution_handles_ids_bandwidth_and_padding() {
        assert_eq!(
            substitute_template(
                "$RepresentationID$/seg-$Number%05d$.m4s",
                Some("v1"),
                Some(1),
                Some(7),
                None
            ),
            Ok("v1/seg-00007.m4s".to_owned())
        );
        assert_eq!(
            substitute_template("$Bandwidth$/x", None, Some(800_000), None, None),
            Ok("800000/x".to_owned())
        );
        assert_eq!(
            substitute_template("cost-$$-x", None, None, None, None),
            Ok("cost-$-x".to_owned())
        );
    }

    #[test]
    fn template_substitution_refuses_missing_or_unknown_tokens() {
        assert_eq!(
            substitute_template("$Number$", None, None, None, None),
            Err(ManifestError::Malformed)
        );
        assert_eq!(
            substitute_template("$Nope$", Some("v0"), None, None, None),
            Err(ManifestError::Malformed)
        );
        assert_eq!(
            substitute_template("$Number", None, None, Some(1), None),
            Err(ManifestError::Malformed)
        );
    }
}
