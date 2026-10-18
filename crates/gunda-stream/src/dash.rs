//! MPEG-DASH manifest parsing.
//!
//! Only the addressing schemes that can be turned into a finite, ordered
//! download plan are interpreted: `SegmentTemplate` (with `$Number$` or a
//! `SegmentTimeline`), inline `SegmentList`, and single-file representations
//! whose `BaseURL` points at one resource. Anything that needs a
//! `sidx` segment index, a live timeline, or track muxing is refused by name
//! rather than guessed at.

use crate::error::{ManifestError, UnsupportedFeature};
use crate::xml::{self, Node};

/// Whether the presentation is finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpdKind {
    Static,
    Dynamic,
}

/// One `<S>` entry of a `SegmentTimeline`, before repeats are expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineEntry {
    time: Option<u64>,
    duration: u64,
    repeat: i64,
}

impl TimelineEntry {
    #[must_use]
    pub const fn time(&self) -> Option<u64> {
        self.time
    }

    #[must_use]
    pub const fn duration(&self) -> u64 {
        self.duration
    }

    #[must_use]
    pub const fn repeat(&self) -> i64 {
        self.repeat
    }
}

/// A `<SegmentTemplate>` at any inheritance level.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SegmentTemplate {
    timescale: Option<u64>,
    duration: Option<u64>,
    start_number: Option<u64>,
    media: Option<String>,
    initialization: Option<String>,
    presentation_time_offset: Option<u64>,
    timeline: Option<Vec<TimelineEntry>>,
}

impl SegmentTemplate {
    #[must_use]
    pub const fn timescale(&self) -> u64 {
        match self.timescale {
            Some(timescale) if timescale > 0 => timescale,
            _ => 1,
        }
    }

    #[must_use]
    pub const fn duration(&self) -> Option<u64> {
        self.duration
    }

    #[must_use]
    pub const fn start_number(&self) -> u64 {
        match self.start_number {
            Some(start_number) => start_number,
            None => 1,
        }
    }

    #[must_use]
    pub fn media(&self) -> Option<&str> {
        self.media.as_deref()
    }

    #[must_use]
    pub fn initialization(&self) -> Option<&str> {
        self.initialization.as_deref()
    }

    #[must_use]
    pub const fn presentation_time_offset(&self) -> u64 {
        match self.presentation_time_offset {
            Some(offset) => offset,
            None => 0,
        }
    }

    #[must_use]
    pub fn timeline(&self) -> Option<&[TimelineEntry]> {
        self.timeline.as_deref()
    }

    /// Fills unset fields from a less specific level, as DASH inheritance requires.
    fn inherit(&mut self, outer: &Self) {
        self.timescale = self.timescale.or(outer.timescale);
        self.duration = self.duration.or(outer.duration);
        self.start_number = self.start_number.or(outer.start_number);
        self.media = self.media.clone().or_else(|| outer.media.clone());
        self.initialization = self
            .initialization
            .clone()
            .or_else(|| outer.initialization.clone());
        self.presentation_time_offset = self
            .presentation_time_offset
            .or(outer.presentation_time_offset);

        if self.timeline.is_none() {
            self.timeline = outer.timeline.clone();
        }
    }
}

/// One `<SegmentURL>` of an inline `SegmentList`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentUrl {
    media: Option<String>,
    media_range: Option<(u64, u64)>,
}

impl SegmentUrl {
    #[must_use]
    pub fn media(&self) -> Option<&str> {
        self.media.as_deref()
    }

    /// Returns the inclusive byte range declared for this segment.
    #[must_use]
    pub const fn media_range(&self) -> Option<(u64, u64)> {
        self.media_range
    }
}

/// The initialization segment declared by a `SegmentList`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentInitialization {
    source_url: String,
    range: Option<(u64, u64)>,
}

impl SegmentInitialization {
    #[must_use]
    pub fn source_url(&self) -> &str {
        &self.source_url
    }

    /// Returns the inclusive byte range declared for the initialization data.
    #[must_use]
    pub const fn range(&self) -> Option<(u64, u64)> {
        self.range
    }
}

/// A `<SegmentList>` at any inheritance level.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SegmentList {
    timescale: Option<u64>,
    duration: Option<u64>,
    start_number: Option<u64>,
    initialization: Option<SegmentInitialization>,
    segment_urls: Option<Vec<SegmentUrl>>,
}

impl SegmentList {
    #[must_use]
    pub const fn timescale(&self) -> u64 {
        match self.timescale {
            Some(timescale) if timescale > 0 => timescale,
            _ => 1,
        }
    }

    #[must_use]
    pub const fn initialization(&self) -> Option<&SegmentInitialization> {
        self.initialization.as_ref()
    }

    #[must_use]
    pub fn segment_urls(&self) -> Option<&[SegmentUrl]> {
        self.segment_urls.as_deref()
    }

    fn inherit(&mut self, outer: &Self) {
        self.timescale = self.timescale.or(outer.timescale);
        self.duration = self.duration.or(outer.duration);
        self.start_number = self.start_number.or(outer.start_number);
        self.initialization = self
            .initialization
            .clone()
            .or_else(|| outer.initialization.clone());

        if self.segment_urls.is_none() {
            self.segment_urls = outer.segment_urls.clone();
        }
    }
}

/// A `<SegmentBase>`, which requires a segment index to enumerate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SegmentBase {
    index_range: Option<String>,
    initialization: Option<String>,
}

impl SegmentBase {
    #[must_use]
    pub fn index_range(&self) -> Option<&str> {
        self.index_range.as_deref()
    }

    #[must_use]
    pub fn initialization(&self) -> Option<&str> {
        self.initialization.as_deref()
    }
}

/// One `<Representation>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Representation {
    id: Option<String>,
    bandwidth: Option<u64>,
    codecs: Option<String>,
    mime_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    base_urls: Vec<String>,
    segment_template: Option<SegmentTemplate>,
    segment_list: Option<SegmentList>,
    segment_base: Option<SegmentBase>,
}

impl Representation {
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
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
    pub fn mime_type(&self) -> Option<&str> {
        self.mime_type.as_deref()
    }

    #[must_use]
    pub fn resolution(&self) -> Option<(u32, u32)> {
        self.width.zip(self.height)
    }

    #[must_use]
    pub fn base_urls(&self) -> &[String] {
        &self.base_urls
    }

    #[must_use]
    pub const fn is_single_file(&self) -> bool {
        self.segment_template.is_none()
            && self.segment_list.is_none()
            && self.segment_base.is_none()
    }

    /// Resolves the addressing for this representation, inheriting from the
    /// enclosing adaptation set and period.
    #[must_use]
    pub fn addressing(&self, set: &AdaptationSet, period: &Period) -> RepresentationAddressing {
        let template = merge_template(
            self.segment_template.as_ref(),
            set.segment_template.as_ref(),
            period.segment_template.as_ref(),
        );
        let list = merge_list(
            self.segment_list.as_ref(),
            set.segment_list.as_ref(),
            period.segment_list.as_ref(),
        );

        match (template, list) {
            (Some(template), _) => RepresentationAddressing::Template(template),
            (None, Some(list)) => RepresentationAddressing::List(list),
            (None, None) => match &self.segment_base {
                Some(base) => RepresentationAddressing::Base(base.clone()),
                None => RepresentationAddressing::SingleFile,
            },
        }
    }
}

/// The addressing scheme resolved for one representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepresentationAddressing {
    Template(SegmentTemplate),
    List(SegmentList),
    Base(SegmentBase),
    SingleFile,
}

fn merge_template(
    inner: Option<&SegmentTemplate>,
    set: Option<&SegmentTemplate>,
    period: Option<&SegmentTemplate>,
) -> Option<SegmentTemplate> {
    let mut merged = inner.cloned().unwrap_or_default();

    let has_any = inner.is_some() || set.is_some() || period.is_some();
    if !has_any {
        return None;
    }

    if let Some(set) = set {
        merged.inherit(set);
    }

    if let Some(period) = period {
        merged.inherit(period);
    }

    Some(merged)
}

fn merge_list(
    inner: Option<&SegmentList>,
    set: Option<&SegmentList>,
    period: Option<&SegmentList>,
) -> Option<SegmentList> {
    let has_any = inner.is_some() || set.is_some() || period.is_some();
    if !has_any {
        return None;
    }

    let mut merged = inner.cloned().unwrap_or_default();

    if let Some(set) = set {
        merged.inherit(set);
    }

    if let Some(period) = period {
        merged.inherit(period);
    }

    Some(merged)
}

/// One `<AdaptationSet>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdaptationSet {
    content_type: Option<String>,
    mime_type: Option<String>,
    lang: Option<String>,
    base_urls: Vec<String>,
    segment_template: Option<SegmentTemplate>,
    segment_list: Option<SegmentList>,
    representations: Vec<Representation>,
}

impl AdaptationSet {
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    #[must_use]
    pub fn mime_type(&self) -> Option<&str> {
        self.mime_type.as_deref()
    }

    #[must_use]
    pub fn language(&self) -> Option<&str> {
        self.lang.as_deref()
    }

    #[must_use]
    pub fn base_urls(&self) -> &[String] {
        &self.base_urls
    }

    #[must_use]
    pub fn representations(&self) -> &[Representation] {
        &self.representations
    }

    /// Returns the declared media kind of this set.
    #[must_use]
    pub fn kind(&self) -> TrackKind {
        if let Some(content_type) = &self.content_type {
            if content_type.eq_ignore_ascii_case("video") {
                return TrackKind::Video;
            }

            if content_type.eq_ignore_ascii_case("audio") {
                return TrackKind::Audio;
            }

            if content_type.eq_ignore_ascii_case("text")
                || content_type.eq_ignore_ascii_case("subtitle")
            {
                return TrackKind::Text;
            }
        }

        let from_mime = self
            .mime_type
            .clone()
            .or_else(|| {
                self.representations
                    .first()
                    .and_then(|representation| representation.mime_type().map(str::to_owned))
            })
            .map(|mime| {
                mime.split('/')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
            });

        match from_mime.as_deref() {
            Some("video") => TrackKind::Video,
            Some("audio") => TrackKind::Audio,
            Some("text") | Some("application") => TrackKind::Text,
            Some(_) | None => {
                if self
                    .representations
                    .iter()
                    .any(|representation| representation.width.is_some())
                {
                    TrackKind::Video
                } else {
                    TrackKind::Unknown
                }
            }
        }
    }
}

/// The declared media kind of an adaptation set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Text,
    Unknown,
}

/// One `<Period>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Period {
    id: Option<String>,
    duration_millis: Option<u64>,
    base_urls: Vec<String>,
    segment_template: Option<SegmentTemplate>,
    segment_list: Option<SegmentList>,
    adaptation_sets: Vec<AdaptationSet>,
}

impl Period {
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    #[must_use]
    pub const fn duration_millis(&self) -> Option<u64> {
        self.duration_millis
    }

    #[must_use]
    pub fn base_urls(&self) -> &[String] {
        &self.base_urls
    }

    #[must_use]
    pub fn adaptation_sets(&self) -> &[AdaptationSet] {
        &self.adaptation_sets
    }

    /// Returns whether the period declares any media at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.adaptation_sets
            .iter()
            .all(|set| set.representations.is_empty())
    }
}

/// A parsed MPD document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mpd {
    kind: MpdKind,
    duration_millis: Option<u64>,
    min_buffer_millis: Option<u64>,
    base_urls: Vec<String>,
    periods: Vec<Period>,
}

impl Mpd {
    #[must_use]
    pub const fn kind(&self) -> MpdKind {
        self.kind
    }

    #[must_use]
    pub const fn duration_millis(&self) -> Option<u64> {
        self.duration_millis
    }

    #[must_use]
    pub const fn min_buffer_millis(&self) -> Option<u64> {
        self.min_buffer_millis
    }

    #[must_use]
    pub fn base_urls(&self) -> &[String] {
        &self.base_urls
    }

    #[must_use]
    pub fn periods(&self) -> &[Period] {
        &self.periods
    }

    /// Returns the only period that carries media.
    ///
    /// Multi-period presentations are refused by the plan builder because
    /// stitching periods requires re-encoding decisions this engine cannot make.
    pub fn media_period(&self) -> Result<&Period, ManifestError> {
        let mut populated = self.periods.iter().filter(|period| !period.is_empty());

        let Some(period) = populated.next() else {
            return Err(ManifestError::Malformed);
        };

        if populated.next().is_some() {
            return Err(UnsupportedFeature::MultiPeriod.into());
        }

        Ok(period)
    }
}

/// Parses an MPD document.
pub fn parse(text: &str) -> Result<Mpd, ManifestError> {
    let root = xml::parse_document(text)?;

    if root.name() != "MPD" {
        return Err(ManifestError::NotAManifest);
    }

    let kind = match root.attribute("type") {
        Some(kind) if kind.eq_ignore_ascii_case("dynamic") => MpdKind::Dynamic,
        _ => MpdKind::Static,
    };

    let duration_millis = root
        .attribute("mediaPresentationDuration")
        .map(parse_iso8601_duration_millis)
        .transpose()?;

    let min_buffer_millis = root
        .attribute("minBufferTime")
        .map(parse_iso8601_duration_millis)
        .transpose()?;

    let base_urls = text_children(&root, "BaseURL");

    let mut periods = Vec::new();

    for period in root.children_named("Period") {
        let duration_millis = period
            .attribute("duration")
            .map(parse_iso8601_duration_millis)
            .transpose()?;

        let mut adaptation_sets = Vec::new();

        for set in period.children_named("AdaptationSet") {
            let mut representations = Vec::new();

            for representation in set.children_named("Representation") {
                representations.push(parse_representation(representation)?);
            }

            adaptation_sets.push(AdaptationSet {
                content_type: set.attribute("contentType").map(str::to_owned),
                mime_type: set.attribute("mimeType").map(str::to_owned),
                lang: set.attribute("lang").map(str::to_owned),
                base_urls: text_children(set, "BaseURL"),
                segment_template: set
                    .child("SegmentTemplate")
                    .map(parse_segment_template)
                    .transpose()?,
                segment_list: set
                    .child("SegmentList")
                    .map(parse_segment_list)
                    .transpose()?,
                representations,
            });
        }

        periods.push(Period {
            id: period.attribute("id").map(str::to_owned),
            duration_millis,
            base_urls: text_children(period, "BaseURL"),
            segment_template: period
                .child("SegmentTemplate")
                .map(parse_segment_template)
                .transpose()?,
            segment_list: period
                .child("SegmentList")
                .map(parse_segment_list)
                .transpose()?,
            adaptation_sets,
        });
    }

    Ok(Mpd {
        kind,
        duration_millis,
        min_buffer_millis,
        base_urls,
        periods,
    })
}

fn parse_representation(node: &Node) -> Result<Representation, ManifestError> {
    Ok(Representation {
        id: node.attribute("id").map(str::to_owned),
        bandwidth: node.attribute_u64("bandwidth")?,
        codecs: node.attribute("codecs").map(str::to_owned),
        mime_type: node.attribute("mimeType").map(str::to_owned),
        width: node.attribute_u32("width")?,
        height: node.attribute_u32("height")?,
        base_urls: text_children(node, "BaseURL"),
        segment_template: node
            .child("SegmentTemplate")
            .map(parse_segment_template)
            .transpose()?,
        segment_list: node
            .child("SegmentList")
            .map(parse_segment_list)
            .transpose()?,
        segment_base: node.child("SegmentBase").map(parse_segment_base),
    })
}

fn parse_segment_template(node: &Node) -> Result<SegmentTemplate, ManifestError> {
    let timeline = node
        .child("SegmentTimeline")
        .map(|timeline| {
            timeline
                .children_named("S")
                .map(|entry| {
                    Ok(TimelineEntry {
                        time: entry.attribute_u64("t")?,
                        duration: entry.attribute_u64("d")?.ok_or(ManifestError::Malformed)?,
                        repeat: entry.attribute_i64("r")?.unwrap_or(0),
                    })
                })
                .collect::<Result<Vec<_>, ManifestError>>()
        })
        .transpose()?;

    Ok(SegmentTemplate {
        timescale: node.attribute_u64("timescale")?,
        duration: node.attribute_u64("duration")?,
        start_number: node.attribute_u64("startNumber")?,
        media: node.attribute("media").map(str::to_owned),
        initialization: node.attribute("initialization").map(str::to_owned),
        presentation_time_offset: node.attribute_u64("presentationTimeOffset")?,
        timeline,
    })
}

fn parse_segment_list(node: &Node) -> Result<SegmentList, ManifestError> {
    let segment_urls = node
        .children_named("SegmentURL")
        .map(|segment| {
            Ok(SegmentUrl {
                media: segment.attribute("media").map(str::to_owned),
                media_range: segment
                    .attribute("mediaRange")
                    .map(parse_byte_range)
                    .transpose()?,
            })
        })
        .collect::<Result<Vec<_>, ManifestError>>()?;

    Ok(SegmentList {
        timescale: node.attribute_u64("timescale")?,
        duration: node.attribute_u64("duration")?,
        start_number: node.attribute_u64("startNumber")?,
        initialization: node
            .child("Initialization")
            .map(|init| -> Result<SegmentInitialization, ManifestError> {
                Ok(SegmentInitialization {
                    source_url: init.attribute("sourceURL").unwrap_or_default().to_owned(),
                    range: init.attribute("range").map(parse_byte_range).transpose()?,
                })
            })
            .transpose()?,
        segment_urls: if segment_urls.is_empty() {
            None
        } else {
            Some(segment_urls)
        },
    })
}

fn parse_segment_base(node: &Node) -> SegmentBase {
    SegmentBase {
        index_range: node.attribute("indexRange").map(str::to_owned),
        initialization: node
            .child("Initialization")
            .and_then(|init| init.attribute("sourceURL"))
            .map(str::to_owned),
    }
}

/// Parses an inclusive `first-last` byte range.
fn parse_byte_range(value: &str) -> Result<(u64, u64), ManifestError> {
    let (first, last) = value
        .split_once('-')
        .ok_or(ManifestError::InvalidByteRange)?;

    let first = first
        .trim()
        .parse::<u64>()
        .map_err(|_| ManifestError::InvalidByteRange)?;

    let last = last
        .trim()
        .parse::<u64>()
        .map_err(|_| ManifestError::InvalidByteRange)?;

    if last < first {
        return Err(ManifestError::InvalidByteRange);
    }

    Ok((first, last))
}

fn text_children(node: &Node, name: &str) -> Vec<String> {
    node.children_named(name)
        .map(|child| child.text().to_owned())
        .filter(|text| !text.is_empty())
        .collect()
}

/// Parses an ISO 8601 duration such as `PT1H2M3.5S` into milliseconds.
pub fn parse_iso8601_duration_millis(value: &str) -> Result<u64, ManifestError> {
    let mut rest = value
        .trim()
        .strip_prefix('P')
        .ok_or(ManifestError::InvalidNumber)?;

    let mut total = 0.0_f64;
    let mut in_time = false;
    let mut number = String::new();
    let mut saw_component = false;

    while let Some(character) = rest.chars().next() {
        rest = &rest[character.len_utf8()..];

        match character {
            'T' => in_time = true,
            '0'..='9' | '.' | ',' => number.push(if character == ',' { '.' } else { character }),
            unit => {
                let magnitude = number
                    .parse::<f64>()
                    .map_err(|_| ManifestError::InvalidNumber)?;
                number.clear();
                saw_component = true;
                total +=
                    unit_seconds(magnitude, unit, in_time).ok_or(ManifestError::InvalidNumber)?;
            }
        }
    }

    // Trailing digits without a unit, an empty duration, and a non-finite total
    // are all malformed.
    if !number.is_empty() || !saw_component || !total.is_finite() || total < 0.0 {
        return Err(ManifestError::InvalidNumber);
    }

    Ok((total * 1000.0).round() as u64)
}

/// Converts one ISO 8601 duration component to seconds.
fn unit_seconds(magnitude: f64, unit: char, in_time: bool) -> Option<f64> {
    Some(match unit {
        'Y' => magnitude * 365.0 * 86_400.0,
        'M' if !in_time => magnitude * 30.0 * 86_400.0,
        'W' => magnitude * 7.0 * 86_400.0,
        'D' => magnitude * 86_400.0,
        'H' => magnitude * 3_600.0,
        'M' => magnitude * 60.0,
        'S' => magnitude,
        _ => return None,
    })
}

/// Expands a `SegmentTimeline` into absolute segment times.
pub fn expand_timeline(
    entries: &[TimelineEntry],
    period_end: Option<u64>,
) -> Result<Vec<u64>, ManifestError> {
    let mut times = Vec::new();
    let mut current: Option<u64> = None;

    for entry in entries {
        if entry.duration == 0 {
            return Err(ManifestError::Malformed);
        }

        let mut time = match entry.time {
            Some(time) => time,
            None => current.ok_or(ManifestError::Malformed)?,
        };

        let repeat = match entry.repeat {
            repeat if repeat >= 0 => repeat as u64,
            -1 => {
                // "Repeat until the period ends" only has meaning when the end
                // is known.
                let end = period_end.ok_or(UnsupportedFeature::IncompletePresentation)?;

                if end <= time {
                    return Err(ManifestError::Malformed);
                }

                ((end - time) / entry.duration).saturating_sub(1)
            }
            _ => return Err(ManifestError::Malformed),
        };

        for _ in 0..=repeat {
            if times.len() >= super::MAX_SEGMENTS {
                return Err(ManifestError::Malformed);
            }

            times.push(time);
            time = time
                .checked_add(entry.duration)
                .ok_or(ManifestError::InvalidNumber)?;
        }

        current = Some(time);
    }

    Ok(times)
}

#[cfg(test)]
mod tests {
    use super::{
        MpdKind, RepresentationAddressing, TrackKind, expand_timeline, parse,
        parse_iso8601_duration_millis,
    };
    use crate::error::{ManifestError, UnsupportedFeature};

    #[test]
    fn parses_iso8601_durations() {
        assert_eq!(parse_iso8601_duration_millis("PT30S"), Ok(30_000));
        assert_eq!(parse_iso8601_duration_millis("PT1H2M3.5S"), Ok(3_723_500));
        assert_eq!(parse_iso8601_duration_millis("P1DT1H"), Ok(90_000_000));
        assert_eq!(parse_iso8601_duration_millis("PT0.5S"), Ok(500));
        assert_eq!(parse_iso8601_duration_millis("PT0S"), Ok(0));
        assert!(parse_iso8601_duration_millis("30S").is_err());
        assert!(parse_iso8601_duration_millis("P").is_err());
        assert!(parse_iso8601_duration_millis("PT").is_err());
    }

    #[test]
    fn expands_timeline_repeats() {
        let text = r#"
        <MPD>
          <Period>
            <AdaptationSet mimeType="video/mp4">
              <SegmentTemplate timescale="90000" media="$Time$.m4s">
                <SegmentTimeline>
                  <S t="0" d="180000" r="2"/>
                  <S d="90000"/>
                </SegmentTimeline>
              </SegmentTemplate>
              <Representation id="v0" bandwidth="1"/>
            </AdaptationSet>
          </Period>
        </MPD>
        "#;

        let mpd = parse(text).expect("manifest must parse");
        let period = mpd.media_period().expect("a period must exist");
        let set = &period.adaptation_sets()[0];
        let representation = &set.representations()[0];

        let RepresentationAddressing::Template(template) = representation.addressing(set, period)
        else {
            panic!("expected template addressing");
        };

        let times = expand_timeline(template.timeline().expect("a timeline must exist"), None)
            .expect("timeline must expand");

        assert_eq!(times, vec![0, 180_000, 360_000, 540_000]);
    }

    #[test]
    fn open_ended_timeline_repeats_need_a_period_end() {
        let entries = [super::TimelineEntry {
            time: Some(0),
            duration: 90_000,
            repeat: -1,
        }];

        assert_eq!(
            expand_timeline(&entries, None),
            Err(ManifestError::Unsupported(
                UnsupportedFeature::IncompletePresentation
            ))
        );
        assert_eq!(
            expand_timeline(&entries, Some(450_000)),
            Ok(vec![0, 90_000, 180_000, 270_000, 360_000])
        );
    }

    #[test]
    fn reads_a_static_presentation_with_inherited_addressing() {
        let text = r#"
        <?xml version="1.0" encoding="utf-8"?>
        <MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT30S" minBufferTime="PT2S">
          <BaseURL>https://cdn.example/vod/</BaseURL>
          <Period id="0">
            <AdaptationSet contentType="video" mimeType="video/mp4" segmentAlignment="true">
              <SegmentTemplate timescale="1000" duration="2000" startNumber="1"
                               initialization="$RepresentationID$/init.mp4"
                               media="$RepresentationID$/seg-$Number$.m4s"/>
              <Representation id="v1" bandwidth="800000" codecs="avc1.4d401e" width="640" height="360"/>
              <Representation id="v2" bandwidth="2400000" codecs="avc1.640028" width="1920" height="1080"/>
            </AdaptationSet>
          </Period>
        </MPD>
        "#;

        let mpd = parse(text).expect("manifest must parse");

        assert_eq!(mpd.kind(), MpdKind::Static);
        assert_eq!(mpd.duration_millis(), Some(30_000));
        assert_eq!(mpd.min_buffer_millis(), Some(2_000));
        assert_eq!(mpd.base_urls(), ["https://cdn.example/vod/"]);

        let period = mpd.media_period().expect("a period must exist");
        let set = &period.adaptation_sets()[0];

        assert_eq!(set.kind(), TrackKind::Video);
        assert_eq!(set.representations().len(), 2);
        assert_eq!(set.representations()[1].resolution(), Some((1920, 1080)));

        let RepresentationAddressing::Template(template) =
            set.representations()[1].addressing(set, period)
        else {
            panic!("expected template addressing");
        };

        assert_eq!(template.timescale(), 1000);
        assert_eq!(template.duration(), Some(2000));
        assert_eq!(template.start_number(), 1);
        assert_eq!(
            template.media(),
            Some("$RepresentationID$/seg-$Number$.m4s")
        );
    }

    #[test]
    fn reads_an_inline_segment_list_with_byte_ranges() {
        let text = r#"
        <MPD type="static">
          <Period>
            <AdaptationSet contentType="video" mimeType="video/mp4">
              <Representation id="v0" bandwidth="1">
                <BaseURL>movie.mp4</BaseURL>
                <SegmentList timescale="1000" duration="4000" startNumber="1">
                  <Initialization sourceURL="movie.mp4" range="0-799"/>
                  <SegmentURL media="movie.mp4" mediaRange="800-4799"/>
                  <SegmentURL media="movie.mp4" mediaRange="4800-8799"/>
                </SegmentList>
              </Representation>
            </AdaptationSet>
          </Period>
        </MPD>
        "#;

        let mpd = parse(text).expect("manifest must parse");
        let period = mpd.media_period().expect("a period must exist");
        let set = &period.adaptation_sets()[0];
        let representation = &set.representations()[0];

        let RepresentationAddressing::List(list) = representation.addressing(set, period) else {
            panic!("expected list addressing");
        };

        let urls = list.segment_urls().expect("segments must exist");
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].media(), Some("movie.mp4"));
        assert_eq!(urls[0].media_range(), Some((800, 4799)));

        let initialization = list.initialization().expect("an init must exist");
        assert_eq!(initialization.source_url(), "movie.mp4");
        assert_eq!(initialization.range(), Some((0, 799)));
    }

    #[test]
    fn detects_a_single_file_representation() {
        let text = r#"
        <MPD type="static">
          <Period>
            <AdaptationSet contentType="audio" mimeType="audio/mp4">
              <Representation id="a0" bandwidth="1">
                <BaseURL>audio.mp4</BaseURL>
              </Representation>
            </AdaptationSet>
          </Period>
        </MPD>
        "#;

        let mpd = parse(text).expect("manifest must parse");
        let period = mpd.media_period().expect("a period must exist");
        let set = &period.adaptation_sets()[0];
        let representation = &set.representations()[0];

        assert!(representation.is_single_file());
        assert_eq!(
            representation.addressing(set, period),
            RepresentationAddressing::SingleFile
        );
        assert_eq!(set.kind(), TrackKind::Audio);
    }

    #[test]
    fn multi_period_presentations_are_refused() {
        let text = r#"
        <MPD type="static">
          <Period id="0"><AdaptationSet mimeType="video/mp4"><Representation id="v0" bandwidth="1"/></AdaptationSet></Period>
          <Period id="1"><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1"/></AdaptationSet></Period>
        </MPD>
        "#;

        let mpd = parse(text).expect("manifest must parse");

        assert_eq!(
            mpd.media_period(),
            Err(ManifestError::Unsupported(UnsupportedFeature::MultiPeriod))
        );
    }

    #[test]
    fn a_document_without_the_mpd_root_is_not_a_manifest() {
        assert_eq!(
            parse("<html><body/></html>"),
            Err(ManifestError::NotAManifest)
        );
    }

    #[test]
    fn malformed_timeline_entries_are_rejected() {
        let text = r#"
        <MPD><Period><AdaptationSet>
          <SegmentTemplate><SegmentTimeline><S t="0"/></SegmentTimeline></SegmentTemplate>
          <Representation id="v0"/>
        </AdaptationSet></Period></MPD>
        "#;

        assert_eq!(parse(text), Err(ManifestError::Malformed));
    }

    #[test]
    fn invalid_byte_ranges_are_rejected() {
        let text = r#"
        <MPD><Period><AdaptationSet>
          <Representation id="v0">
            <SegmentList><SegmentURL media="a.mp4" mediaRange="100-99"/></SegmentList>
          </Representation>
        </AdaptationSet></Period></MPD>
        "#;

        assert_eq!(parse(text), Err(ManifestError::InvalidByteRange));
    }
}
