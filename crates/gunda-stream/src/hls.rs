//! HTTP Live Streaming playlist parsing.
//!
//! Only the tags needed to build a download plan are interpreted. Unknown tags
//! are ignored, which is what the HLS specification requires of clients.

use crate::error::ManifestError;

/// A byte range within a resource, as declared by `EXT-X-BYTERANGE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    length: u64,
    offset: Option<u64>,
}

impl ByteRange {
    #[must_use]
    pub const fn new(length: u64, offset: Option<u64>) -> Self {
        Self { length, offset }
    }

    #[must_use]
    pub const fn length(self) -> u64 {
        self.length
    }

    #[must_use]
    pub const fn offset(self) -> Option<u64> {
        self.offset
    }

    fn parse(value: &str) -> Result<Self, ManifestError> {
        let (length, offset) = match value.split_once('@') {
            Some((length, offset)) => (length, Some(offset)),
            None => (value, None),
        };

        let length = length
            .trim()
            .parse::<u64>()
            .map_err(|_| ManifestError::InvalidByteRange)?;

        if length == 0 {
            return Err(ManifestError::InvalidByteRange);
        }

        let offset = offset
            .map(str::trim)
            .map(|offset| offset.parse::<u64>())
            .transpose()
            .map_err(|_| ManifestError::InvalidByteRange)?;

        Ok(Self { length, offset })
    }
}

/// A resource reference that may address the whole resource or one range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRef {
    uri: String,
    byte_range: Option<ByteRange>,
}

impl ResourceRef {
    #[must_use]
    pub fn new(uri: impl Into<String>, byte_range: Option<ByteRange>) -> Self {
        Self {
            uri: uri.into(),
            byte_range,
        }
    }

    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    #[must_use]
    pub const fn byte_range(&self) -> Option<ByteRange> {
        self.byte_range
    }
}

/// The encryption declared for a media segment.
///
/// The engine does not decrypt, but it must be able to recognize encryption and
/// refuse the plan instead of publishing ciphertext as a finished video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRef {
    method: String,
    uri: Option<String>,
    iv: Option<String>,
}

impl KeyRef {
    #[must_use]
    pub fn new(method: impl Into<String>, uri: Option<String>, iv: Option<String>) -> Self {
        Self {
            method: method.into(),
            uri,
            iv,
        }
    }

    /// Returns whether this key describes actual encryption.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        !self.method.eq_ignore_ascii_case("NONE")
    }

    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    #[must_use]
    pub fn key_uri(&self) -> Option<&str> {
        self.uri.as_deref()
    }

    #[must_use]
    pub fn iv(&self) -> Option<&str> {
        self.iv.as_deref()
    }
}

/// One media segment of a media playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    resource: ResourceRef,
    duration_millis: Option<u64>,
    media_sequence: u64,
    init: Option<ResourceRef>,
    key: Option<KeyRef>,
    discontinuity: bool,
}

impl Segment {
    #[must_use]
    pub fn resource(&self) -> &ResourceRef {
        &self.resource
    }

    #[must_use]
    pub const fn duration_millis(&self) -> Option<u64> {
        self.duration_millis
    }

    #[must_use]
    pub const fn media_sequence(&self) -> u64 {
        self.media_sequence
    }

    /// Returns the initialization segment that applies to this segment.
    #[must_use]
    pub fn init(&self) -> Option<&ResourceRef> {
        self.init.as_ref()
    }

    #[must_use]
    pub fn key(&self) -> Option<&KeyRef> {
        self.key.as_ref()
    }

    #[must_use]
    pub const fn has_discontinuity(&self) -> bool {
        self.discontinuity
    }
}

/// A parsed media playlist.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaPlaylist {
    segments: Vec<Segment>,
    target_duration_secs: Option<u64>,
    ended: bool,
}

impl MediaPlaylist {
    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    #[must_use]
    pub const fn target_duration_secs(&self) -> Option<u64> {
        self.target_duration_secs
    }

    /// Returns whether the playlist declared `EXT-X-ENDLIST`.
    #[must_use]
    pub const fn is_ended(&self) -> bool {
        self.ended
    }

    /// Returns whether any segment is encrypted.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.segments
            .iter()
            .any(|segment| segment.key.as_ref().is_some_and(KeyRef::is_encrypted))
    }

    /// Returns the declared total duration in milliseconds when every segment
    /// declares one.
    #[must_use]
    pub fn total_duration_millis(&self) -> Option<u64> {
        self.segments
            .iter()
            .map(Segment::duration_millis)
            .try_fold(0_u64, |total, duration| {
                duration.map(|duration| total.saturating_add(duration))
            })
    }
}

/// One `EXT-X-STREAM-INF` variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    uri: String,
    bandwidth: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    codecs: Option<String>,
    audio_group: Option<String>,
}

impl Variant {
    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    #[must_use]
    pub const fn bandwidth(&self) -> Option<u64> {
        self.bandwidth
    }

    #[must_use]
    pub const fn width(&self) -> Option<u32> {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> Option<u32> {
        self.height
    }

    #[must_use]
    pub fn codecs(&self) -> Option<&str> {
        self.codecs.as_deref()
    }

    #[must_use]
    pub fn audio_group(&self) -> Option<&str> {
        self.audio_group.as_deref()
    }

    #[must_use]
    pub fn resolution(&self) -> Option<(u32, u32)> {
        self.width.zip(self.height)
    }
}

/// One `EXT-X-MEDIA` rendition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendition {
    kind: String,
    group_id: Option<String>,
    name: Option<String>,
    language: Option<String>,
    uri: Option<String>,
    default: bool,
}

impl Rendition {
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    #[must_use]
    pub fn group_id(&self) -> Option<&str> {
        self.group_id.as_deref()
    }

    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    #[must_use]
    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    /// Returns the rendition playlist, absent for muxed renditions.
    #[must_use]
    pub fn uri(&self) -> Option<&str> {
        self.uri.as_deref()
    }

    #[must_use]
    pub const fn is_default(&self) -> bool {
        self.default
    }
}

/// A parsed master playlist.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MasterPlaylist {
    variants: Vec<Variant>,
    renditions: Vec<Rendition>,
    iframe_variants: Vec<Variant>,
}

impl MasterPlaylist {
    #[must_use]
    pub fn variants(&self) -> &[Variant] {
        &self.variants
    }

    #[must_use]
    pub fn renditions(&self) -> &[Rendition] {
        &self.renditions
    }

    /// Returns the variants that carry only key frames.
    #[must_use]
    pub fn iframe_variants(&self) -> &[Variant] {
        &self.iframe_variants
    }

    /// Selects the highest-bandwidth variant, preferring declared resolutions.
    ///
    /// Bandwidth is the only ordering signal every master playlist provides.
    #[must_use]
    pub fn best_variant(&self) -> Option<&Variant> {
        self.variants.iter().max_by_key(|variant| {
            (
                variant.bandwidth.unwrap_or(0),
                variant.width.unwrap_or(0),
                variant.height.unwrap_or(0),
            )
        })
    }

    /// Returns the default audio rendition of a variant's audio group.
    #[must_use]
    pub fn default_audio_rendition(&self, variant: &Variant) -> Option<&Rendition> {
        let group = variant.audio_group()?;

        self.renditions
            .iter()
            .filter(|rendition| {
                rendition.kind.eq_ignore_ascii_case("AUDIO")
                    && rendition.group_id.as_deref() == Some(group)
                    && rendition.uri.is_some()
            })
            .min_by_key(|rendition| !rendition.default)
    }
}

/// A parsed playlist of either kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playlist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

/// Attribute names and values from one tag line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Attributes {
    pairs: Vec<(String, String)>,
}

impl Attributes {
    fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn get_u64(&self, name: &str) -> Result<Option<u64>, ManifestError> {
        self.get(name)
            .map(|value| {
                value
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| ManifestError::InvalidNumber)
            })
            .transpose()
    }

    fn is_yes(&self, name: &str) -> bool {
        self.get(name)
            .is_some_and(|value| value.eq_ignore_ascii_case("YES"))
    }
}

/// Splits a tag's attribute list, respecting double-quoted values.
fn parse_attributes(input: &str) -> Result<Attributes, ManifestError> {
    let mut pairs = Vec::new();
    let mut rest = input;

    while !rest.is_empty() {
        let Some((name, tail)) = rest.split_once('=') else {
            return Err(ManifestError::Malformed);
        };

        let name = name.trim();

        // Attribute names carry no spaces; a space means an unquoted value ran
        // past its comma, which makes the whole list ambiguous.
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(ManifestError::Malformed);
        }

        let (value, next) = if let Some(quoted) = tail.strip_prefix('"') {
            let Some((value, tail)) = quoted.split_once('"') else {
                return Err(ManifestError::Malformed);
            };

            let tail = tail.trim_start();
            let next = match tail.strip_prefix(',') {
                Some(next) => next,
                None if tail.is_empty() => "",
                None => return Err(ManifestError::Malformed),
            };

            (value, next)
        } else {
            // An unquoted value ends at its comma, which `split_once` consumes.
            match tail.split_once(',') {
                Some((value, next)) => (value, next),
                None => (tail, ""),
            }
        };

        pairs.push((name.to_owned(), value.to_owned()));
        rest = next.trim_start();
    }

    Ok(Attributes { pairs })
}

fn parse_duration_millis(value: &str) -> Result<u64, ManifestError> {
    let seconds = value
        .trim()
        .parse::<f64>()
        .map_err(|_| ManifestError::InvalidNumber)?;

    if !seconds.is_finite() || seconds < 0.0 {
        return Err(ManifestError::InvalidNumber);
    }

    // Round to the nearest millisecond; totals stay well inside u64 for any
    // realistic presentation length.
    Ok((seconds * 1000.0).round() as u64)
}

fn parse_resolution(value: &str) -> Result<(u32, u32), ManifestError> {
    let (width, height) = value.split_once('x').ok_or(ManifestError::Malformed)?;

    Ok((
        width
            .trim()
            .parse::<u32>()
            .map_err(|_| ManifestError::InvalidNumber)?,
        height
            .trim()
            .parse::<u32>()
            .map_err(|_| ManifestError::InvalidNumber)?,
    ))
}

/// Parses an HLS playlist of either kind.
pub fn parse(text: &str) -> Result<Playlist, ManifestError> {
    // The specification requires the first line of every playlist to be the
    // `#EXTM3U` header; requiring it is also what keeps arbitrary text (an HTML
    // error page, a JSON body) from being read as one giant segment list.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    match text.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some("#EXTM3U") => {}
        _ => return Err(ManifestError::NotAManifest),
    }

    let mut master = MasterPlaylist::default();
    let mut media = MediaPlaylist::default();

    let mut pending_stream_inf: Option<Attributes> = None;
    let mut pending_duration: Option<u64> = None;
    let mut pending_range: Option<ByteRange> = None;
    let mut discontinuity = false;

    let mut current_key: Option<KeyRef> = None;
    let mut current_map: Option<ResourceRef> = None;
    let mut media_sequence = 0_u64;

    // `EXT-X-BYTERANGE` may omit its offset when consecutive segments address
    // one resource, so the end of the previous range must be remembered.
    let mut last_range_end: Option<(String, u64)> = None;

    let mut saw_master_tag = false;
    let mut saw_media_tag = false;

    for line in text.lines() {
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        let Some(tag) = line.strip_prefix('#') else {
            if line.starts_with("data:") {
                // Unsupported, and ignoring it would silently truncate output.
                return Err(ManifestError::Unsupported(
                    crate::error::UnsupportedFeature::DataUriSegments,
                ));
            }

            let Some(attributes) = pending_stream_inf.take() else {
                // A media segment line.
                let pending = pending_range.take();

                let offset = match pending {
                    Some(range) => match range.offset {
                        Some(offset) => Some(offset),
                        None => last_range_end
                            .as_ref()
                            .filter(|(uri, _)| uri.as_str() == line)
                            .map(|(_, end)| *end),
                    },
                    None => None,
                };

                let byte_range = pending.map(|range| ByteRange::new(range.length, offset));

                if let Some(range) = byte_range {
                    last_range_end = Some((line.to_owned(), offset.unwrap_or(0) + range.length()));
                } else {
                    last_range_end = None;
                }

                saw_media_tag = true;
                if media.segments.len() >= crate::MAX_SEGMENTS {
                    return Err(ManifestError::TooManySegments);
                }
                media.segments.push(Segment {
                    resource: ResourceRef::new(line, byte_range),
                    duration_millis: pending_duration.take(),
                    media_sequence,
                    init: current_map.clone(),
                    key: current_key.clone(),
                    discontinuity,
                });
                media_sequence = media_sequence.saturating_add(1);
                discontinuity = false;
                continue;
            };

            if pending_range.take().is_some() {
                // A variant URI never carries a byterange; the tag belonged to a
                // segment that never arrived.
                return Err(ManifestError::Malformed);
            }

            master
                .variants
                .push(attributes_to_variant(&attributes, line)?);
            saw_master_tag = true;
            continue;
        };

        let (name, value) = match tag.split_once(':') {
            Some((name, value)) => (name, Some(value)),
            None => (tag, None),
        };

        match name {
            "EXTM3U" => {}
            "EXT-X-VERSION" => {}
            "EXT-X-STREAM-INF" => {
                pending_stream_inf = Some(parse_attributes(value.unwrap_or_default())?);
                saw_master_tag = true;
            }
            "EXT-X-I-FRAME-STREAM-INF" => {
                let attributes = parse_attributes(value.unwrap_or_default())?;
                let uri = attributes
                    .get("URI")
                    .ok_or(ManifestError::Malformed)?
                    .to_owned();
                master
                    .iframe_variants
                    .push(attributes_to_variant(&attributes, &uri)?);
                saw_master_tag = true;
            }
            "EXT-X-MEDIA" => {
                let attributes = parse_attributes(value.unwrap_or_default())?;
                master.renditions.push(Rendition {
                    kind: attributes
                        .get("TYPE")
                        .ok_or(ManifestError::Malformed)?
                        .to_owned(),
                    group_id: attributes.get("GROUP-ID").map(str::to_owned),
                    name: attributes.get("NAME").map(str::to_owned),
                    language: attributes.get("LANGUAGE").map(str::to_owned),
                    uri: attributes.get("URI").map(str::to_owned),
                    default: attributes.is_yes("DEFAULT"),
                });
                saw_master_tag = true;
            }
            "EXTINF" => {
                let value = value.ok_or(ManifestError::Malformed)?;
                let duration = value.split(',').next().unwrap_or_default();
                pending_duration = Some(parse_duration_millis(duration)?);
                saw_media_tag = true;
            }
            "EXT-X-BYTERANGE" => {
                pending_range = Some(ByteRange::parse(value.ok_or(ManifestError::Malformed)?)?);
                saw_media_tag = true;
            }
            "EXT-X-TARGETDURATION" => {
                media.target_duration_secs = Some(
                    value
                        .ok_or(ManifestError::Malformed)?
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| ManifestError::InvalidNumber)?,
                );
                saw_media_tag = true;
            }
            "EXT-X-MEDIA-SEQUENCE" => {
                media_sequence = value
                    .ok_or(ManifestError::Malformed)?
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| ManifestError::InvalidNumber)?;
                saw_media_tag = true;
            }
            "EXT-X-ENDLIST" => {
                media.ended = true;
                saw_media_tag = true;
            }
            "EXT-X-DISCONTINUITY" => {
                discontinuity = true;
                saw_media_tag = true;
            }
            "EXT-X-KEY" => {
                let attributes = parse_attributes(value.ok_or(ManifestError::Malformed)?)?;
                let method = attributes
                    .get("METHOD")
                    .ok_or(ManifestError::Malformed)?
                    .to_owned();

                current_key = if method.eq_ignore_ascii_case("NONE") {
                    None
                } else {
                    Some(KeyRef::new(
                        method,
                        attributes.get("URI").map(str::to_owned),
                        attributes.get("IV").map(str::to_owned),
                    ))
                };
                saw_media_tag = true;
            }
            "EXT-X-MAP" => {
                let attributes = parse_attributes(value.ok_or(ManifestError::Malformed)?)?;
                let uri = attributes
                    .get("URI")
                    .ok_or(ManifestError::Malformed)?
                    .to_owned();
                let byte_range = attributes
                    .get("BYTERANGE")
                    .map(ByteRange::parse)
                    .transpose()?;
                current_map = Some(ResourceRef::new(uri, byte_range));
                saw_media_tag = true;
            }
            _ => {}
        }
    }

    if pending_stream_inf.is_some() {
        // A variant tag with no URI is a truncated playlist.
        return Err(ManifestError::Malformed);
    }

    match (saw_master_tag, saw_media_tag) {
        (true, true) => Err(ManifestError::AmbiguousPlaylist),
        (true, false) => Ok(Playlist::Master(master)),
        (false, true) => Ok(Playlist::Media(media)),
        (false, false) => Err(ManifestError::NotAManifest),
    }
}

fn attributes_to_variant(attributes: &Attributes, uri: &str) -> Result<Variant, ManifestError> {
    let resolution = attributes
        .get("RESOLUTION")
        .map(parse_resolution)
        .transpose()?;

    Ok(Variant {
        uri: uri.to_owned(),
        bandwidth: attributes.get_u64("BANDWIDTH")?,
        width: resolution.map(|(width, _)| width),
        height: resolution.map(|(_, height)| height),
        codecs: attributes.get("CODECS").map(str::to_owned),
        audio_group: attributes.get("AUDIO").map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::{ByteRange, KeyRef, Playlist, parse};
    use crate::error::{ManifestError, UnsupportedFeature};

    fn media(text: &str) -> super::MediaPlaylist {
        match parse(text).expect("playlist must parse") {
            Playlist::Media(playlist) => playlist,
            Playlist::Master(_) => panic!("expected a media playlist"),
        }
    }

    #[test]
    fn media_playlist_reads_segments_and_metadata() {
        let playlist = media(
            "#EXTM3U\n\
             #EXT-X-VERSION:6\n\
             #EXT-X-TARGETDURATION:10\n\
             #EXT-X-MEDIA-SEQUENCE:7\n\
             #EXTINF:9.009,first\n\
             seg1.m4s\n\
             #EXTINF:5.5,\n\
             seg2.m4s\n\
             #EXT-X-ENDLIST\n",
        );

        assert_eq!(playlist.segments().len(), 2);
        assert_eq!(playlist.target_duration_secs(), Some(10));
        assert!(playlist.is_ended());
        assert!(!playlist.is_encrypted());
        assert_eq!(playlist.total_duration_millis(), Some(14_509));

        let first = &playlist.segments()[0];
        assert_eq!(first.resource().uri(), "seg1.m4s");
        assert_eq!(first.media_sequence(), 7);
        assert_eq!(playlist.segments()[1].media_sequence(), 8);
        assert_eq!(first.duration_millis(), Some(9_009));
    }

    #[test]
    fn unfinished_playlist_is_reported_as_not_ended() {
        let playlist = media("#EXTM3U\n#EXTINF:4,\none.ts\n");

        assert!(!playlist.is_ended());
        assert_eq!(playlist.total_duration_millis(), Some(4_000));
    }

    #[test]
    fn byterange_continuation_uses_the_previous_end() {
        let playlist = media(
            "#EXTM3U\n\
             #EXTINF:4,\n\
             #EXT-X-BYTERANGE:1000@0\n\
             all.ts\n\
             #EXTINF:4,\n\
             #EXT-X-BYTERANGE:2000\n\
             all.ts\n\
             #EXTINF:4,\n\
             other.ts\n",
        );

        assert_eq!(
            playlist.segments()[0].resource().byte_range(),
            Some(ByteRange::new(1000, Some(0)))
        );
        assert_eq!(
            playlist.segments()[1].resource().byte_range(),
            Some(ByteRange::new(2000, Some(1000)))
        );
        // A different resource cannot continue the previous range.
        assert_eq!(playlist.segments()[2].resource().byte_range(), None);
    }

    #[test]
    fn init_map_and_key_apply_to_following_segments() {
        let playlist = media(
            "#EXTM3U\n\
             #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x1\n\
             #EXTINF:4,\n\
             seg1.m4s\n\
             #EXT-X-KEY:METHOD=NONE\n\
             #EXTINF:4,\n\
             seg2.m4s\n",
        );

        assert_eq!(
            playlist.segments()[0].init().map(|init| init.uri()),
            Some("init.mp4")
        );
        assert_eq!(
            playlist.segments()[0]
                .init()
                .and_then(|init| init.byte_range()),
            Some(ByteRange::new(720, Some(0)))
        );
        assert_eq!(
            playlist.segments()[0].key().map(KeyRef::method),
            Some("AES-128")
        );
        assert!(
            playlist.segments()[0]
                .key()
                .is_some_and(KeyRef::is_encrypted)
        );
        assert!(playlist.segments()[1].key().is_none());
        assert!(playlist.is_encrypted());
    }

    #[test]
    fn discontinuity_is_attached_to_the_following_segment() {
        let playlist = media(
            "#EXTM3U\n\
             #EXTINF:4,\n\
             a.ts\n\
             #EXT-X-DISCONTINUITY\n\
             #EXTINF:4,\n\
             b.ts\n",
        );

        assert!(!playlist.segments()[0].has_discontinuity());
        assert!(playlist.segments()[1].has_discontinuity());
    }

    #[test]
    fn master_playlist_selects_the_highest_bandwidth_variant() {
        let playlist = parse(
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"aud\"\n\
             low.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1920x1080\n\
             high.m3u8\n\
             #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=100000,URI=\"iframe.m3u8\"\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",URI=\"en.m3u8\",DEFAULT=YES,LANGUAGE=en\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"German\",URI=\"de.m3u8\",LANGUAGE=de\n",
        )
        .expect("master playlist must parse");

        let Playlist::Master(master) = playlist else {
            panic!("expected a master playlist");
        };

        assert_eq!(master.variants().len(), 2);
        assert_eq!(master.iframe_variants().len(), 1);

        let best = master.best_variant().expect("a variant must exist");
        assert_eq!(best.uri(), "high.m3u8");
        assert_eq!(best.bandwidth(), Some(2_400_000));
        assert_eq!(best.resolution(), Some((1920, 1080)));

        let low = &master.variants()[0];
        assert_eq!(low.codecs(), Some("avc1.4d401e,mp4a.40.2"));
        assert_eq!(low.audio_group(), Some("aud"));

        let audio = master
            .default_audio_rendition(low)
            .expect("a default audio rendition must resolve");
        assert_eq!(audio.name(), Some("English"));
        assert_eq!(audio.uri(), Some("en.m3u8"));
    }

    #[test]
    fn master_playlist_rejects_a_variant_tag_without_a_uri() {
        let result = parse("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n");

        assert_eq!(result, Err(ManifestError::Malformed));
    }

    #[test]
    fn mixed_playlists_are_rejected() {
        let result = parse(
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=1\n\
             v.m3u8\n\
             #EXTINF:4,\n\
             s.ts\n",
        );

        assert_eq!(result, Err(ManifestError::AmbiguousPlaylist));
    }

    #[test]
    fn non_manifest_text_is_rejected() {
        assert_eq!(parse("<html></html>"), Err(ManifestError::NotAManifest));
    }

    #[test]
    fn zero_length_byterange_is_rejected() {
        let result = parse("#EXTM3U\n#EXT-X-BYTERANGE:0@0\nseg.ts\n");

        assert_eq!(result, Err(ManifestError::InvalidByteRange));
    }

    #[test]
    fn attribute_values_may_contain_commas_and_equals_signs() {
        let playlist = parse(
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=1,CODECS=\"a,b=c\",RESOLUTION=1x2\n\
             v.m3u8\n",
        )
        .expect("master playlist must parse");

        let Playlist::Master(master) = playlist else {
            panic!("expected a master playlist");
        };

        assert_eq!(master.variants()[0].codecs(), Some("a,b=c"));
    }

    #[test]
    fn malformed_attribute_lists_are_rejected() {
        for text in [
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH\nv.m3u8\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=\"unterminated\nv.m3u8\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1 EXTRA=2\nv.m3u8\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=abc\nv.m3u8\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=640\nv.m3u8\n",
        ] {
            assert!(
                matches!(
                    parse(text),
                    Err(ManifestError::Malformed | ManifestError::InvalidNumber)
                ),
                "unexpected result for {text}"
            );
        }
    }

    #[test]
    fn unknown_tags_are_ignored() {
        let playlist = media(
            "#EXTM3U\n\
             #EXT-X-INDEPENDENT-SEGMENTS\n\
             #EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n\
             #EXT-X-DATERANGE:ID=\"x\",START-DATE=\"2026-01-01T00:00:00Z\"\n\
             #EXTINF:4,\n\
             s.ts\n",
        );

        assert_eq!(playlist.segments().len(), 1);
    }

    #[test]
    fn data_uri_segments_are_refused_rather_than_dropped() {
        let result = parse("#EXTM3U\n#EXTINF:4,\ndata:text/plain,hello\n");

        assert_eq!(
            result,
            Err(ManifestError::Unsupported(
                UnsupportedFeature::DataUriSegments
            ))
        );
    }
}
