//! Compact, URL-free selectors for one track of a presentation.
//!
//! The selector is persisted with the job, so it must stay small and must not
//! carry signed playlist URLs. A manifest is read again when the job executes;
//! the selector only names which variant or representation to use and records
//! enough declared metadata to detect that the presentation changed.

use gunda_core::download::ExecutionSelection;

use crate::error::ManifestError;
use crate::hls::Variant;

const HLS_VERSION: &str = "1";
const DASH_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HlsSelection {
    pub(crate) variant: usize,
    pub(crate) bandwidth: Option<u64>,
    pub(crate) resolution: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DashSelection {
    pub(crate) video_set: Option<usize>,
    pub(crate) video_representation: Option<usize>,
    pub(crate) video_bandwidth: Option<u64>,
    pub(crate) audio_set: Option<usize>,
    pub(crate) audio_representation: Option<usize>,
    pub(crate) audio_bandwidth: Option<u64>,
}

pub(crate) fn hls_selection(
    variant_index: usize,
    variant: &Variant,
) -> Result<ExecutionSelection, ManifestError> {
    let (width, height) = match variant.resolution() {
        Some((width, height)) => (width.to_string(), height.to_string()),
        None => ("-".to_owned(), "-".to_owned()),
    };

    ExecutionSelection::new(format!(
        "hls:{HLS_VERSION}:{variant_index}:{}:{width}:{height}",
        optional_number(variant.bandwidth())
    ))
    .map_err(|_| ManifestError::Malformed)
}

pub(crate) fn dash_selection(
    video: Option<(usize, usize, Option<u64>)>,
    audio: Option<(usize, usize, Option<u64>)>,
) -> Result<ExecutionSelection, ManifestError> {
    let (video_set, video_representation, video_bandwidth) = video.map_or_else(
        || ("-".to_owned(), "-".to_owned(), "-".to_owned()),
        |(set, representation, bandwidth)| {
            (
                set.to_string(),
                representation.to_string(),
                optional_number(bandwidth),
            )
        },
    );
    let (audio_set, audio_representation, audio_bandwidth) = audio.map_or_else(
        || ("-".to_owned(), "-".to_owned(), "-".to_owned()),
        |(set, representation, bandwidth)| {
            (
                set.to_string(),
                representation.to_string(),
                optional_number(bandwidth),
            )
        },
    );

    ExecutionSelection::new(format!(
        "dash:{DASH_VERSION}:{video_set}:{video_representation}:{video_bandwidth}:{audio_set}:{audio_representation}:{audio_bandwidth}"
    ))
    .map_err(|_| ManifestError::Malformed)
}

pub(crate) fn parse_hls(selection: &ExecutionSelection) -> Result<HlsSelection, ManifestError> {
    let mut parts = selection.as_str().split(':');

    if parts.next() != Some("hls") || parts.next() != Some(HLS_VERSION) {
        return Err(ManifestError::SelectionUnavailable);
    }

    let variant = number(parts.next())?;
    let bandwidth = optional_number_value(parts.next())?;
    let width = optional_number_value(parts.next())?;
    let height = optional_number_value(parts.next())?;

    if parts.next().is_some() {
        return Err(ManifestError::SelectionUnavailable);
    }

    let resolution = match (width, height) {
        (Some(width), Some(height)) => Some((
            u32::try_from(width).map_err(|_| ManifestError::SelectionUnavailable)?,
            u32::try_from(height).map_err(|_| ManifestError::SelectionUnavailable)?,
        )),
        (None, None) => None,
        _ => return Err(ManifestError::SelectionUnavailable),
    };

    Ok(HlsSelection {
        variant,
        bandwidth,
        resolution,
    })
}

pub(crate) fn parse_dash(selection: &ExecutionSelection) -> Result<DashSelection, ManifestError> {
    let mut parts = selection.as_str().split(':');

    if parts.next() != Some("dash") || parts.next() != Some(DASH_VERSION) {
        return Err(ManifestError::SelectionUnavailable);
    }

    let video_set = optional_usize(parts.next())?;
    let video_representation = optional_usize(parts.next())?;
    let video_bandwidth = optional_number_value(parts.next())?;
    let audio_set = optional_usize(parts.next())?;
    let audio_representation = optional_usize(parts.next())?;
    let audio_bandwidth = optional_number_value(parts.next())?;

    if parts.next().is_some() {
        return Err(ManifestError::SelectionUnavailable);
    }

    Ok(DashSelection {
        video_set,
        video_representation,
        video_bandwidth,
        audio_set,
        audio_representation,
        audio_bandwidth,
    })
}

fn optional_number(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn number(value: Option<&str>) -> Result<usize, ManifestError> {
    value
        .ok_or(ManifestError::SelectionUnavailable)?
        .parse()
        .map_err(|_| ManifestError::SelectionUnavailable)
}

fn optional_usize(value: Option<&str>) -> Result<Option<usize>, ManifestError> {
    match value {
        Some("-") | None => Ok(None),
        Some(value) => value
            .parse()
            .map(Some)
            .map_err(|_| ManifestError::SelectionUnavailable),
    }
}

fn optional_number_value(value: Option<&str>) -> Result<Option<u64>, ManifestError> {
    match value {
        Some("-") | None => Ok(None),
        Some(value) => value
            .parse()
            .map(Some)
            .map_err(|_| ManifestError::SelectionUnavailable),
    }
}

#[cfg(test)]
mod tests {
    use gunda_core::download::ExecutionSelection;

    use super::{dash_selection, hls_selection, parse_dash, parse_hls};
    use crate::hls::{self, Playlist};

    #[test]
    fn hls_selection_round_trips_declared_metadata() {
        let Playlist::Master(master) = hls::parse(
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=4000000,RESOLUTION=1920x1080\n\
             high.m3u8\n",
        )
        .unwrap() else {
            panic!("the fixture is a master playlist");
        };
        let selection = hls_selection(3, &master.variants()[0]).unwrap();

        assert_eq!(selection.as_str(), "hls:1:3:4000000:1920:1080");
        let parsed = parse_hls(&selection).unwrap();

        assert_eq!(parsed.variant, 3);
        assert_eq!(parsed.bandwidth, Some(4_000_000));
        assert_eq!(parsed.resolution, Some((1920, 1080)));
    }

    #[test]
    fn dash_selection_round_trips_both_tracks() {
        let selection =
            dash_selection(Some((1, 4, Some(2_000_000))), Some((2, 0, Some(128_000)))).unwrap();

        assert_eq!(selection.as_str(), "dash:1:1:4:2000000:2:0:128000");
        let parsed = parse_dash(&selection).unwrap();

        assert_eq!(parsed.video_set, Some(1));
        assert_eq!(parsed.video_representation, Some(4));
        assert_eq!(parsed.audio_set, Some(2));
        assert_eq!(parsed.audio_representation, Some(0));
    }

    #[test]
    fn another_engine_selector_is_refused() {
        let selection = ExecutionSelection::new("http:1:segments").unwrap();

        assert!(parse_hls(&selection).is_err());
        assert!(parse_dash(&selection).is_err());
    }
}
