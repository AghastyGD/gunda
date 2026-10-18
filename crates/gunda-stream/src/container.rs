//! The container the planned bytes actually are.
//!
//! A segmented engine concatenates the resources a manifest names. What comes
//! out is not a file of the manifest's format: an `EXT-X-MAP` initialization
//! segment followed by `.m4s` media segments is a fragmented MP4, and a plain
//! `EXTINF` playlist of `.ts` segments is an MPEG-2 transport stream. Naming the
//! output `.m3u8` after the playlist, or `.mp4` regardless of the bytes, would
//! misdescribe the file.
//!
//! So the container is read from the leading bytes of the first resource. What
//! this module never does is guess: a stream it cannot recognise is reported as
//! unknown and keeps a neutral extension, rather than being labelled as a format
//! that a player would then refuse.

/// Number of leading bytes inspected when recognising a container.
///
/// Enough for the ISO base media box header, the EBML header with its document
/// type, and three transport-stream sync bytes 188 bytes apart.
pub const SNIFF_BYTES: usize = 2048;

/// A media container Gunda can recognise and name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// ISO base media (MP4), including the fragmented form HLS and DASH use.
    Mp4,
    /// ISO base media carrying audio only.
    M4a,
    /// MPEG-2 transport stream.
    Ts,
    /// Matroska.
    Matroska,
    /// WebM, Matroska's constrained subset.
    Webm,
    /// ADTS-framed AAC.
    Aac,
    /// MPEG audio, typically layer III.
    Mp3,
}

impl Container {
    /// The file extension for this container, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::M4a => "m4a",
            Self::Ts => "ts",
            Self::Matroska => "mkv",
            Self::Webm => "webm",
            Self::Aac => "aac",
            Self::Mp3 => "mp3",
        }
    }

    /// Whether this container is an ISO base media file.
    ///
    /// Fragmented MP4 tracks can be muxed into one file as they stand; a
    /// transport stream cannot without being demuxed first.
    #[must_use]
    pub const fn is_iso_base_media(self) -> bool {
        matches!(self, Self::Mp4 | Self::M4a)
    }

    /// Whether the muxer can read this container as one track of an output.
    ///
    /// ISO base media is carried over as it stands. A transport stream and an
    /// ADTS audio file are demuxed into samples first. Matroska and MPEG audio
    /// would need a demuxer this crate does not have, so they are refused
    /// rather than published without their other track.
    #[must_use]
    pub const fn can_be_muxed(self) -> bool {
        matches!(self, Self::Mp4 | Self::M4a | Self::Ts | Self::Aac)
    }
}

/// Combines what the leading bytes show with what the manifest declared.
///
/// The bytes win whenever they are recognisable: a manifest that calls its
/// segments ISO base media while the origin serves transport-stream packets is
/// describing them wrongly, and the file must be named after what it is. The
/// declaration only fills in what the bytes cannot establish on their own, so a
/// stream whose origin could not be read at all still gets the right extension.
#[must_use]
pub fn detect(head: &[u8], hint: Option<Container>) -> Option<Container> {
    sniff(head, hint == Some(Container::M4a)).or(hint)
}

/// Recognises the container from the leading bytes of a media resource.
///
/// `audio_only` is what the manifest said about the track being downloaded: the
/// ISO base media format is the same for a video and an audio presentation, so
/// only the caller knows which tracks are coming. An explicit audio brand in the
/// bytes overrides it, because that is evidence rather than a declaration.
#[must_use]
pub fn sniff(head: &[u8], audio_only: bool) -> Option<Container> {
    iso_base_media(head, audio_only)
        .or_else(|| matroska(head))
        .or_else(|| transport_stream(head))
        .or_else(|| mpeg_audio(head))
}

/// Reads an ISO base media file type box, as `ftyp` at offset four.
///
/// A fragmented presentation's initialization segment also carries the handler
/// of every track it declares, which is the one place the bytes themselves say
/// whether they are audio only.
fn iso_base_media(head: &[u8], audio_only: bool) -> Option<Container> {
    if head.get(4..8)? != b"ftyp" {
        return None;
    }

    // A track handler in the bytes is evidence; the brand and the manifest's own
    // declaration are weaker, and only decide when no handler was found.
    match tracks_are_audio_only(head) {
        Some(true) => Some(Container::M4a),
        Some(false) => Some(Container::Mp4),
        None if is_audio_brand(head.get(8..12)) || audio_only => Some(Container::M4a),
        None => Some(Container::Mp4),
    }
}

/// Whether a file type box names an audio-only brand.
fn is_audio_brand(brand: Option<&[u8]>) -> bool {
    matches!(brand, Some(b"M4A " | b"M4B " | b"M4P " | b"F4A "))
}

/// Whether every track the initialization segment declares is an audio track.
///
/// Returns `None` when no handler is present in the inspected bytes, which is
/// normal for a plain (non-fragmented) MP4 whose metadata sits at the end.
fn tracks_are_audio_only(head: &[u8]) -> Option<bool> {
    let mut audio = false;

    for index in 4..head.len().saturating_sub(4) {
        if &head[index..index + 4] != b"hdlr" {
            continue;
        }

        // A handler box is at least this long, and this is not a general-purpose
        // string search: the box size before the type has to look like one.
        let size = u32::from_be_bytes([
            head[index - 4],
            head[index - 3],
            head[index - 2],
            head[index - 1],
        ]);

        if !(8..=4096).contains(&size) {
            continue;
        }

        // The handler type follows the version and flags, then a predefined
        // field, so it starts twelve bytes after the box type.
        let Some(handler) = head.get(index + 12..index + 16) else {
            continue;
        };

        match handler {
            b"vide" => return Some(false),
            b"soun" => audio = true,
            _ => {}
        }
    }

    audio.then_some(true)
}

/// Reads the EBML magic and the Matroska document type.
fn matroska(head: &[u8]) -> Option<Container> {
    if !head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return None;
    }

    // The document type string is near the head of the header; scanning for it
    // avoids implementing EBML variable-length integer reading for one value.
    let webm = head.windows(4).take(16).any(|window| window == b"webm");

    Some(if webm {
        Container::Webm
    } else {
        Container::Matroska
    })
}

/// Recognises a transport stream by its sync byte, 188 bytes apart.
///
/// One sync byte is weak evidence: `0x47` is a plausible first byte for other
/// data too. Two more at the fixed interval are not.
fn transport_stream(head: &[u8]) -> Option<Container> {
    if head.first() != Some(&0x47) {
        return None;
    }

    let packet = 188;

    for offset in [packet, packet * 2, packet * 3] {
        if head.len() <= offset {
            // Fewer bytes than the interval we wanted to confirm: the first sync
            // byte is all the evidence this resource can give.
            return Some(Container::Ts);
        }

        if head[offset] != 0x47 {
            return None;
        }
    }

    Some(Container::Ts)
}

/// Recognises ADTS-framed AAC and MPEG audio from their frame headers.
///
/// A packed-audio segment can carry an ID3v2 tag in front of its frames, which
/// says when the segment plays rather than what codec it holds. The tag is
/// stepped over: `ID3` alone is evidence of a tag, never of MPEG audio.
fn mpeg_audio(head: &[u8]) -> Option<Container> {
    let audio = match id3v2_length(head) {
        Some(length) => head.get(length..)?,
        None => head,
    };

    let (first, second) = (*audio.first()?, *audio.get(1)?);

    if first != 0xFF {
        return None;
    }

    // ADTS: a 12-bit sync word, then a two-bit layer field that must be zero.
    if (second & 0xF6) == 0xF0 {
        return Some(Container::Aac);
    }

    // MPEG audio: the same sync word with a non-zero layer field.
    if (second & 0xE0) == 0xE0 && (second & 0x06) != 0 {
        return Some(Container::Mp3);
    }

    None
}

/// The length of an ID3v2 tag at the start of the bytes, when one is there.
///
/// An ID3v2 header is ten bytes: the identifier, two version bytes, a flags
/// byte and a size whose four bytes each carry seven bits. A footer, when the
/// flags announce one, adds another ten bytes.
pub(crate) fn id3v2_length(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" {
        return None;
    }

    let size = (usize::from(bytes[6] & 0x7F) << 21)
        | (usize::from(bytes[7] & 0x7F) << 14)
        | (usize::from(bytes[8] & 0x7F) << 7)
        | usize::from(bytes[9] & 0x7F);
    let footer = if bytes[5] & 0x10 != 0 { 10 } else { 0 };

    Some(10 + size + footer)
}

#[cfg(test)]
mod tests {
    use super::{Container, SNIFF_BYTES, sniff};

    /// An ISO base media header with the given brand.
    fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
        let mut head = vec![0, 0, 0, 0x18];
        head.extend_from_slice(b"ftyp");
        head.extend_from_slice(brand);
        head.extend_from_slice(&[0, 0, 0, 0]);
        head
    }

    /// A handler box for one track kind.
    fn handler(kind: &[u8; 4]) -> Vec<u8> {
        let mut box_bytes = vec![0, 0, 0, 0x21];
        box_bytes.extend_from_slice(b"hdlr");
        box_bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        box_bytes.extend_from_slice(kind);
        box_bytes
    }

    #[test]
    fn recognises_a_video_container_from_its_file_type() {
        assert_eq!(sniff(&ftyp(b"isom"), false), Some(Container::Mp4));
        assert_eq!(sniff(&ftyp(b"dash"), false), Some(Container::Mp4));
        assert_eq!(sniff(&ftyp(b"iso5"), false), Some(Container::Mp4));
    }

    #[test]
    fn an_audio_brand_is_audio_even_when_the_manifest_expected_video() {
        assert_eq!(sniff(&ftyp(b"M4A "), false), Some(Container::M4a));
    }

    #[test]
    fn a_track_handler_decides_between_mp4_and_m4a() {
        let mut audio_only = ftyp(b"iso5");
        audio_only.extend_from_slice(&handler(b"soun"));

        let mut with_video = ftyp(b"iso5");
        with_video.extend_from_slice(&handler(b"vide"));
        with_video.extend_from_slice(&handler(b"soun"));

        assert_eq!(sniff(&audio_only, false), Some(Container::M4a));
        assert_eq!(sniff(&with_video, false), Some(Container::Mp4));
        // The manifest's own declaration is used when the bytes do not say.
        assert_eq!(sniff(&ftyp(b"iso5"), true), Some(Container::M4a));
    }

    #[test]
    fn recognises_a_transport_stream_from_three_sync_bytes() {
        let mut head = vec![0_u8; SNIFF_BYTES];

        for offset in [0, 188, 376, 564] {
            head[offset] = 0x47;
        }

        assert_eq!(sniff(&head, false), Some(Container::Ts));

        // A single sync byte is not enough evidence on its own.
        head[188] = 0x00;

        assert_eq!(sniff(&head, false), None);
    }

    #[test]
    fn recognises_matroska_and_webm() {
        let mut doc_type = vec![0x1A, 0x45, 0xDF, 0xA3];
        doc_type.extend_from_slice(&[0x42, 0x86, 0x81, 0x01]);
        doc_type.extend_from_slice(b"webm");

        assert_eq!(sniff(&doc_type, false), Some(Container::Webm));

        doc_type.truncate(4);
        doc_type.extend_from_slice(b"matroska");

        assert_eq!(sniff(&doc_type, false), Some(Container::Matroska));
    }

    #[test]
    fn recognises_adts_aac_and_mpeg_audio() {
        // ADTS: sync, layer 0, protection absent.
        assert_eq!(
            sniff(&[0xFF, 0xF1, 0x50, 0x80], false),
            Some(Container::Aac)
        );
        // MPEG-1 layer III: sync, layer bits non-zero.
        assert_eq!(
            sniff(&[0xFF, 0xFB, 0x90, 0x00], false),
            Some(Container::Mp3)
        );

        // An ID3 tag describes the frames after it, not the codec itself.
        let mut tagged_aac = vec![b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, 4];
        tagged_aac.extend_from_slice(&[0; 4]);
        tagged_aac.extend_from_slice(&[0xFF, 0xF1, 0x50, 0x80]);
        assert_eq!(sniff(&tagged_aac, false), Some(Container::Aac));

        let mut tagged_mp3 = vec![b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, 4];
        tagged_mp3.extend_from_slice(&[0; 4]);
        tagged_mp3.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        assert_eq!(sniff(&tagged_mp3, false), Some(Container::Mp3));

        // A tag with no frame behind it says nothing about the codec.
        assert_eq!(sniff(b"ID3\x04\x00\x00\x00", false), None);
    }

    #[test]
    fn unknown_bytes_are_not_guessed_at() {
        assert_eq!(sniff(&[], false), None);
        assert_eq!(sniff(b"<html>not media</html>", false), None);
        assert_eq!(sniff(&[0x00; 64], false), None);
        assert_eq!(sniff(&[0xFF, 0x00, 0x00, 0x00], false), None);

        for container in [
            Container::Mp4,
            Container::M4a,
            Container::Ts,
            Container::Matroska,
            Container::Webm,
            Container::Aac,
            Container::Mp3,
        ] {
            assert!(!container.extension().is_empty());
        }
    }
}
