//! Muxing the separate tracks of one presentation into a single MP4 file.
//!
//! Some presentations arrive as two tracks, one carrying the picture and one
//! carrying the sound. Concatenating them produces nothing a player can use: a
//! file has to be built that describes both at once. This module does that for
//! the case Gunda meets.
//!
//! A fragmented MP4 input is what MPEG-DASH and HLS with an initialization
//! segment deliver: an initialization segment followed by one fragment per
//! media segment. Its sample entry and codec configuration boxes are carried
//! over untouched, so H.264, HEVC, AV1, VP9, AAC and Opus all travel the same
//! path.
//!
//! HLS without an initialization segment delivers an MPEG-2 transport stream
//! instead, and a packed-audio rendition can be ADTS-framed AAC. Neither is a
//! file with a sample table, so those inputs are demuxed first and the sample
//! entry is built from the codec configuration they carry. Only H.264 and AAC
//! are read that way, because those are the codecs a transport stream on the
//! web actually holds.
//!
//! The output is a plain MP4 with real sample tables rather than another
//! fragmented file, because that is what players, editors and system metadata
//! readers open without a manifest to guide them.

use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter, SeekFrom};

use crate::container::{Container, SNIFF_BYTES};
use crate::sniff;
use crate::ts::{self, Codec, ElementaryTrack};

/// The largest initialization or fragment box this will read into memory.
///
/// Both carry metadata rather than media, so anything larger is a malformed or
/// hostile file rather than a segment to be handled.
const MAX_METADATA_BOX: u64 = 16 * 1024 * 1024;

/// How much of a track is copied at a time.
const COPY_BUFFER: usize = 256 * 1024;

/// The identity transformation matrix, in the 16.16 and 2.30 fixed point the
/// format uses.
const IDENTITY_MATRIX: [u8; 36] = [
    0x00, 0x01, 0x00, 0x00, // 1.0
    0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, //
    0x00, 0x01, 0x00, 0x00, // 1.0
    0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, //
    0x40, 0x00, 0x00, 0x00, // 16384.0
];

/// What can go wrong while muxing two tracks into one file.
#[derive(Debug)]
pub enum MuxError {
    /// A track file could not be read or the output could not be written.
    Io(io::Error),

    /// A track is not shaped the way its format requires.
    Malformed(&'static str),

    /// A track is shaped correctly but in a way this muxer does not write.
    Unsupported(&'static str),

    /// The result would not fit in the file this muxer writes.
    TooLarge,
}

impl fmt::Display for MuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "the track could not be read: {error}"),
            Self::Malformed(what) => write!(f, "the track does not describe its samples: {what}"),
            Self::Unsupported(what) => write!(f, "the track cannot be combined: {what}"),
            Self::TooLarge => write!(f, "the combined media is larger than one file can hold"),
        }
    }
}

impl Error for MuxError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// Muxes a video track and an audio track into one MP4 file.
///
/// Each input is read for what its bytes are: a fragmented MP4 is taken as it
/// stands, a transport stream or an ADTS audio file is demuxed into an
/// elementary track first. The tracks are written one after the other, which
/// the sample tables describe exactly. Returns the size of the file written.
pub async fn mux_mp4(video: &Path, audio: &Path, output: &Path) -> Result<u64, MuxError> {
    mux_mp4_with_layout(video, audio, output, false).await
}

async fn mux_mp4_with_layout(
    video: &Path,
    audio: &Path,
    output: &Path,
    force_large_offsets: bool,
) -> Result<u64, MuxError> {
    let mut temporary = TemporaryTracks::default();

    let tracks = vec![
        read_track(video, &mut temporary).await?,
        read_track(audio, &mut temporary).await?,
    ];

    write_mp4_layout(output, &tracks, force_large_offsets).await
}

/// Reads one input as a track, demuxing it when it is not already ISO media.
///
/// The container is decided by the bytes, not by what the manifest claimed: an
/// HLS playlist that uses `EXT-X-MAP` is documented as fragmented MP4, but only
/// the origin's bytes say what it actually served.
async fn read_track(path: &Path, temporary: &mut TemporaryTracks) -> Result<Track, MuxError> {
    let head = read_head(path).await?;

    match sniff(&head, false) {
        Some(Container::Ts) => {
            let elementary = temporary.reserve(path);
            let source = path.to_path_buf();
            let extracted = elementary.clone();

            let track = tokio::task::spawn_blocking(move || ts::demux(&source, &extracted))
                .await
                .map_err(|_| {
                    MuxError::Malformed("the transport stream was not read to the end")
                })??;

            Ok(Track {
                path: elementary,
                demuxed: DemuxedTrack::from_elementary(track)?,
            })
        }
        Some(Container::Aac) => {
            let elementary = temporary.reserve(path);
            let source = path.to_path_buf();
            let extracted = elementary.clone();

            let track = tokio::task::spawn_blocking(move || ts::demux_adts(&source, &extracted))
                .await
                .map_err(|_| MuxError::Malformed("the audio file was not read to the end"))??;

            Ok(Track {
                path: elementary,
                demuxed: DemuxedTrack::from_elementary(track)?,
            })
        }
        _ => Ok(Track {
            path: path.to_path_buf(),
            demuxed: demux(path).await?,
        }),
    }
}

/// Reads the leading bytes a container is recognised by.
async fn read_head(path: &Path) -> Result<Vec<u8>, MuxError> {
    let mut file = File::open(path).await.map_err(MuxError::Io)?;
    let mut head = vec![0_u8; SNIFF_BYTES];
    let read = file.read(&mut head).await.map_err(MuxError::Io)?;

    head.truncate(read);

    Ok(head)
}

/// The elementary files a demuxed input was written to.
///
/// They are staging files exactly like the tracks themselves and must not
/// outlive the mux: whatever happens, they are removed when this is dropped.
#[derive(Default)]
struct TemporaryTracks {
    paths: Vec<PathBuf>,
}

impl TemporaryTracks {
    /// Names the elementary file for one input.
    ///
    /// It sits next to the input, whose name already identifies the download
    /// and the track, so two inputs can never ask for the same file.
    fn reserve(&mut self, source: &Path) -> PathBuf {
        let mut name = source.file_name().unwrap_or_default().to_owned();
        name.push(".elementary");

        let path = source.with_file_name(name);
        self.paths.push(path.clone());

        path
    }
}

impl Drop for TemporaryTracks {
    fn drop(&mut self) {
        for path in &self.paths {
            // Best effort: a leftover elementary file wastes space, but failing
            // a completed download over one would be worse.
            let _ = fs::remove_file(path);
        }
    }
}

/// A demuxed track, and where its samples can be read from.
struct Track {
    path: PathBuf,
    demuxed: DemuxedTrack,
}

/// One sample of a track: where its bytes are, and how it plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
    /// Offset of the first byte in the track the sample was read from.
    offset: u64,
    size: u32,
    /// Decode duration, in the track's timescale.
    duration: u32,
    /// Presentation time minus decode time, when the sample is reordered.
    composition_offset: i32,
    /// Whether the sample stands on its own.
    sync: bool,
}

/// A track read from an initialization segment and the fragments that follow.
struct DemuxedTrack {
    /// The track header, ready to have its identifier and duration replaced.
    tkhd: Vec<u8>,
    /// The media header, ready to have its duration replaced.
    mdhd: Vec<u8>,
    hdlr: Vec<u8>,
    /// The media information header, whatever kind it is.
    media_header: ([u8; 4], Vec<u8>),
    dinf: Vec<u8>,
    stsd: Vec<u8>,
    timescale: u32,
    samples: Vec<Sample>,
    /// Decode time just after the last sample, in the track's timescale.
    duration: u64,
}

impl DemuxedTrack {
    /// The number of bytes every sample of this track adds up to.
    fn sample_bytes(&self) -> u64 {
        self.samples
            .iter()
            .map(|sample| u64::from(sample.size))
            .sum()
    }

    /// The track's duration in another timescale.
    fn duration_in(&self, timescale: u32) -> u64 {
        self.duration.saturating_mul(u64::from(timescale)) / u64::from(self.timescale)
    }

    /// Builds the description of a track that was demuxed from an elementary
    /// stream rather than read from ISO media.
    ///
    /// A transport stream and an ADTS file carry no sample entry, no media
    /// header and no handler, so every box an ISO reader needs is written here
    /// from the codec configuration the demuxer recovered: the picture size
    /// from the sequence parameter set, the channel count and sample rate from
    /// the ADTS header, and the codec configuration in the sample entry.
    fn from_elementary(track: ElementaryTrack) -> Result<Self, MuxError> {
        let ElementaryTrack {
            codec,
            timescale,
            samples,
            duration,
        } = track;

        let bytes: u64 = samples.iter().map(|sample| u64::from(sample.size)).sum();

        let samples = samples
            .iter()
            .map(|sample| Sample {
                offset: sample.offset,
                size: sample.size,
                duration: sample.duration,
                composition_offset: sample.composition_offset,
                sync: sample.sync,
            })
            .collect();

        let (handler, media_header, stsd, width, height, volume) = match &codec {
            Codec::Avc {
                configuration,
                width,
                height,
            } => (
                *b"vide",
                (*b"vmhd", full_box_payload(1, &[0; 8])),
                sample_table(&video_sample_entry(configuration, *width, *height)),
                *width,
                *height,
                0,
            ),
            Codec::Aac {
                configuration,
                channel_count,
                sample_rate,
            } => (
                *b"soun",
                (*b"smhd", full_box_payload(0, &[0; 4])),
                sample_table(&audio_sample_entry(
                    *configuration,
                    *channel_count,
                    *sample_rate,
                    average_bitrate(bytes, duration, timescale),
                )),
                0,
                0,
                0x0100,
            ),
        };

        Ok(Self {
            tkhd: track_header_body(volume, width, height),
            mdhd: media_header_body(timescale),
            hdlr: handler_body(handler),
            media_header,
            dinf: dinf_body(),
            stsd,
            timescale,
            samples,
            duration,
        })
    }
}

/// An average bitrate, in bits per second, for a sample entry that has to state
/// one and has no declared rate to copy.
fn average_bitrate(bytes: u64, duration: u64, timescale: u32) -> u32 {
    if duration == 0 || timescale == 0 {
        return 0;
    }

    let bits = bytes.saturating_mul(8).saturating_mul(u64::from(timescale));

    u32::try_from(bits / duration).unwrap_or(u32::MAX)
}

/// One box: its size, its type and its body.
fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len().saturating_add(8));

    out.extend_from_slice(&(body.len() as u32).saturating_add(8).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);

    out
}

/// A full box body: a version, then flags, then the fields the box declares.
fn full_box_payload(flags: u32, body: &[u8]) -> Vec<u8> {
    let mut payload = vec![0];
    payload.extend_from_slice(&flags.to_be_bytes()[1..]);
    payload.extend_from_slice(body);

    payload
}

/// A complete full box: its size, its type and the payload.
fn full_box_body(kind: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
    boxed(kind, &full_box_payload(flags, body))
}

/// The body of a track header for a picture or a sound, with the identifier and
/// duration left for the writer to fill in.
fn track_header_body(volume: u16, width: u16, height: u16) -> Vec<u8> {
    let mut body = vec![0, 0, 0, 7]; // version, then enabled | in movie | in preview
    body.extend_from_slice(&[0; 4]); // creation time
    body.extend_from_slice(&[0; 4]); // modification time
    body.extend_from_slice(&[0; 4]); // track identifier, replaced later
    body.extend_from_slice(&[0; 4]); // reserved
    body.extend_from_slice(&[0; 4]); // duration, replaced later
    body.extend_from_slice(&[0; 8]); // reserved
    body.extend_from_slice(&[0; 2]); // layer
    body.extend_from_slice(&[0; 2]); // alternate group
    body.extend_from_slice(&volume.to_be_bytes());
    body.extend_from_slice(&[0; 2]); // reserved
    body.extend_from_slice(&IDENTITY_MATRIX);
    body.extend_from_slice(&u32::from(width).wrapping_shl(16).to_be_bytes());
    body.extend_from_slice(&u32::from(height).wrapping_shl(16).to_be_bytes());

    body
}

/// The body of a media header, with the duration left for the writer.
fn media_header_body(timescale: u32) -> Vec<u8> {
    let mut body = vec![0, 0, 0, 0]; // version and flags
    body.extend_from_slice(&[0; 4]); // creation time
    body.extend_from_slice(&[0; 4]); // modification time
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(&[0; 4]); // duration, replaced later
    body.extend_from_slice(&[0x55, 0xC4]); // language: und
    body.extend_from_slice(&[0, 0]); // pre_defined

    body
}

/// The body of a handler box for one track kind.
fn handler_body(kind: [u8; 4]) -> Vec<u8> {
    let mut body = vec![0, 0, 0, 0]; // version and flags
    body.extend_from_slice(&[0; 4]); // pre_defined
    body.extend_from_slice(&kind);
    body.extend_from_slice(&[0; 12]); // reserved
    body.extend_from_slice(if &kind == b"vide" {
        b"VideoHandler\0"
    } else {
        b"SoundHandler\0"
    });

    body
}

/// The body of the data information box a track needs.
fn dinf_body() -> Vec<u8> {
    // One self-contained data reference, which is what a file with its media
    // beside its headers carries.
    let mut dref = vec![0, 0, 0, 0]; // version and flags
    dref.extend_from_slice(&1_u32.to_be_bytes()); // entry count
    dref.extend_from_slice(&full_box_body(b"url ", 1, &[]));

    boxed(b"dref", &dref)
}

/// A sample description table holding one sample entry.
fn sample_table(entry: &[u8]) -> Vec<u8> {
    let mut body = vec![0, 0, 0, 0]; // version and flags
    body.extend_from_slice(&1_u32.to_be_bytes()); // one entry
    body.extend_from_slice(entry);

    body
}

/// The `avc1` sample entry for an H.264 track.
fn video_sample_entry(configuration: &[u8], width: u16, height: u16) -> Vec<u8> {
    let mut body = vec![0; 6]; // reserved
    body.extend_from_slice(&1_u16.to_be_bytes()); // data reference index
    body.extend_from_slice(&[0; 16]); // pre_defined and reserved
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&height.to_be_bytes());
    body.extend_from_slice(&0x0048_0000_u32.to_be_bytes()); // 72 dpi horizontally
    body.extend_from_slice(&0x0048_0000_u32.to_be_bytes()); // 72 dpi vertically
    body.extend_from_slice(&[0; 4]); // reserved
    body.extend_from_slice(&1_u16.to_be_bytes()); // one frame per sample
    body.extend_from_slice(&[0; 32]); // compressor name
    body.extend_from_slice(&0x0018_u16.to_be_bytes()); // colour depth
    body.extend_from_slice(&0xFFFF_u16.to_be_bytes()); // pre_defined
    body.extend_from_slice(&boxed(b"avcC", configuration));
    body.extend_from_slice(&boxed(b"pasp", &[0, 0, 0, 1, 0, 0, 0, 1]));

    boxed(b"avc1", &body)
}

/// The `mp4a` sample entry for an AAC track.
fn audio_sample_entry(
    configuration: u16,
    channel_count: u16,
    sample_rate: u32,
    average_bitrate: u32,
) -> Vec<u8> {
    let mut body = vec![0; 6]; // reserved
    body.extend_from_slice(&1_u16.to_be_bytes()); // data reference index
    body.extend_from_slice(&[0; 8]); // reserved
    body.extend_from_slice(&channel_count.to_be_bytes());
    body.extend_from_slice(&16_u16.to_be_bytes()); // sample size
    body.extend_from_slice(&[0; 4]); // pre_defined and reserved
    body.extend_from_slice(&sample_rate.wrapping_shl(16).to_be_bytes());
    body.extend_from_slice(&esds_body(configuration, average_bitrate));

    boxed(b"mp4a", &body)
}

/// The `esds` box describing an AAC decoder.
fn esds_body(configuration: u16, average_bitrate: u32) -> Vec<u8> {
    let mut decoder = Vec::new();
    decoder.push(0x40); // MPEG-4 audio
    decoder.push(0x15); // audio stream, no upstream, reserved bit set
    decoder.extend_from_slice(&[0; 3]); // buffer size
    decoder.extend_from_slice(&average_bitrate.to_be_bytes()); // maximum rate
    decoder.extend_from_slice(&average_bitrate.to_be_bytes()); // average rate
    descriptor(&mut decoder, 0x05, &configuration.to_be_bytes());

    let mut es = Vec::new();
    es.extend_from_slice(&[0; 2]); // ES identifier
    es.push(0); // no stream dependency, no URL, no OCR
    descriptor(&mut es, 0x04, &decoder);
    descriptor(&mut es, 0x06, &[0x02]); // an empty SL config

    let mut body = Vec::new();
    descriptor(&mut body, 0x03, &es);

    full_box_body(b"esds", 0, &body)
}

/// Appends an MPEG-4 descriptor: its tag, its length and its payload.
fn descriptor(out: &mut Vec<u8>, tag: u8, payload: &[u8]) {
    out.push(tag);

    let length = payload.len();

    if length < 0x80 {
        out.push(length as u8);
    } else {
        out.push(0x80 | ((length >> 21) as u8 & 0x7F));
        out.push(0x80 | ((length >> 14) as u8 & 0x7F));
        out.push(0x80 | ((length >> 7) as u8 & 0x7F));
        out.push(length as u8 & 0x7F);
    }

    out.extend_from_slice(payload);
}

/// Reads one track: its initialization segment, and every sample of its
/// fragments.
async fn demux(path: &Path) -> Result<DemuxedTrack, MuxError> {
    let mut file = File::open(path).await.map_err(MuxError::Io)?;
    let length = file.metadata().await.map_err(MuxError::Io)?.len();

    let mut initialization = None;
    let mut fragments = Fragments::default();
    let mut position = 0_u64;

    while position.saturating_add(8) <= length {
        let header = read_header(&mut file, position, length).await?;

        match &header.kind {
            b"moov" => initialization = Some(read_body(&mut file, &header).await?),
            b"moof" => {
                let body = read_body(&mut file, &header).await?;
                fragments.push(&body, header.start)?;
            }
            _ => {}
        }

        position = header.end();
    }

    let initialization =
        initialization.ok_or(MuxError::Malformed("there is no initialization segment"))?;

    read_initialization(&initialization, fragments)
}

/// One box of a track file.
struct Header {
    kind: [u8; 4],
    /// Offset of the box's first byte.
    start: u64,
    /// Offset of the box's first payload byte.
    body: u64,
    /// Payload length.
    length: u64,
}

impl Header {
    /// Offset just past the box.
    fn end(&self) -> u64 {
        self.body.saturating_add(self.length)
    }
}

/// Reads one box header at `position`.
///
/// A box whose size is expressed in 64 bits is refused rather than guessed at:
/// the segments this muxer meets never use one, and reading the wrong number of
/// bytes would corrupt the result instead of failing.
async fn read_header(file: &mut File, position: u64, file_length: u64) -> Result<Header, MuxError> {
    let mut head = [0_u8; 8];

    file.seek(SeekFrom::Start(position))
        .await
        .map_err(MuxError::Io)?;
    file.read_exact(&mut head).await.map_err(MuxError::Io)?;

    let size = u32::from_be_bytes(head[..4].try_into().expect("four bytes"));
    let kind = head[4..8].try_into().expect("four bytes");

    match size {
        // A size of zero means the box runs to the end of the file.
        0 => Ok(Header {
            kind,
            start: position,
            body: position.saturating_add(8),
            length: file_length.saturating_sub(position.saturating_add(8)),
        }),
        1 => Err(MuxError::Malformed("a box states its size in 64 bits")),
        size if size < 8 => Err(MuxError::Malformed("a box states an impossible size")),
        size => Ok(Header {
            kind,
            start: position,
            body: position.saturating_add(8),
            length: u64::from(size) - 8,
        }),
    }
}

/// Reads a box's payload into memory.
async fn read_body(file: &mut File, header: &Header) -> Result<Vec<u8>, MuxError> {
    if header.length > MAX_METADATA_BOX {
        return Err(MuxError::Malformed("a metadata box is implausibly large"));
    }

    let length = usize::try_from(header.length).map_err(|_| MuxError::TooLarge)?;
    let mut body = vec![0_u8; length];

    file.seek(SeekFrom::Start(header.body))
        .await
        .map_err(MuxError::Io)?;
    file.read_exact(&mut body).await.map_err(MuxError::Io)?;

    Ok(body)
}

/// The boxes directly inside one box.
struct Boxes<'a> {
    rest: &'a [u8],
}

impl<'a> Boxes<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self { rest: body }
    }
}

impl<'a> Iterator for Boxes<'a> {
    type Item = ([u8; 4], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let size = u32::from_be_bytes(self.rest.get(..4)?.try_into().ok()?) as usize;
        let kind = self.rest.get(4..8)?.try_into().ok()?;

        // A size of zero means "to the end of the box that contains it".
        let size = if size == 0 { self.rest.len() } else { size };

        if size < 8 || size > self.rest.len() {
            self.rest = &[];
            return None;
        }

        let body = &self.rest[8..size];
        self.rest = &self.rest[size..];

        Some((kind, body))
    }
}

/// Finds the first box reached by following `path` inside `body`.
fn find_box<'a>(body: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
    let (first, rest) = path.split_first()?;

    Boxes::new(body).find_map(|(kind, inner)| {
        if kind != **first {
            return None;
        }

        if rest.is_empty() {
            Some(inner)
        } else {
            find_box(inner, rest)
        }
    })
}

/// Reads everything a track needs in order to be written again.
fn read_initialization(moov: &[u8], fragments: Fragments) -> Result<DemuxedTrack, MuxError> {
    let missing = || MuxError::Malformed("the initialization segment describes no track");
    let track = find_box(moov, &[b"trak"]).ok_or_else(missing)?;

    let tkhd = find_box(track, &[b"tkhd"]).ok_or_else(missing)?.to_vec();

    let mdhd = find_box(track, &[b"mdia", b"mdhd"])
        .ok_or_else(missing)?
        .to_vec();
    let timescale = read_timescale(&mdhd).ok_or_else(missing)?;

    if timescale == 0 {
        return Err(MuxError::Malformed(
            "the media header declares no timescale",
        ));
    }

    let hdlr = find_box(track, &[b"mdia", b"hdlr"])
        .ok_or_else(missing)?
        .to_vec();

    let media_header = [b"vmhd", b"smhd", b"nmhd"]
        .into_iter()
        .find_map(|kind| {
            find_box(track, &[b"mdia", b"minf", kind]).map(|body| (*kind, body.to_vec()))
        })
        .ok_or_else(missing)?;

    let dinf = find_box(track, &[b"mdia", b"minf", b"dinf"])
        .ok_or_else(missing)?
        .to_vec();

    let stsd = find_box(track, &[b"mdia", b"minf", b"stbl", b"stsd"])
        .ok_or_else(missing)?
        .to_vec();

    if fragments.samples.is_empty() {
        return Err(MuxError::Malformed("the track carries no samples"));
    }

    Ok(DemuxedTrack {
        tkhd,
        mdhd,
        hdlr,
        media_header,
        dinf,
        stsd,
        timescale,
        samples: fragments.samples,
        duration: fragments.decoded,
    })
}

/// The samples read from a track's fragments.
#[derive(Default)]
struct Fragments {
    samples: Vec<Sample>,
    /// Decode time of the next sample, relative to `origin`.
    decoded: u64,
    /// The absolute decode time of the first fragment, which the track is
    /// re-based to so that the file starts where its media does.
    origin: Option<u64>,
}

impl Fragments {
    /// Reads one fragment box, whose first byte is at `start`.
    fn push(&mut self, moof: &[u8], start: u64) -> Result<(), MuxError> {
        for (kind, traf) in Boxes::new(moof) {
            if &kind == b"traf" {
                self.push_traf(traf, start)?;
            }
        }

        Ok(())
    }

    fn push_traf(&mut self, traf: &[u8], fragment: u64) -> Result<(), MuxError> {
        let header = TrackHeader::read(
            find_box(traf, &[b"tfhd"])
                .ok_or(MuxError::Malformed("a fragment has no track header"))?,
        )?;

        // Only the first sample entry of the initialization segment is written,
        // so a fragment that selects another one would be silently wrong.
        if header
            .sample_description_index
            .is_some_and(|index| index != 1)
        {
            return Err(MuxError::Unsupported(
                "a fragment selects a second sample entry",
            ));
        }

        if let Some(base) = find_box(traf, &[b"tfdt"]).and_then(read_decode_time) {
            self.align(base)?;
        }

        // A run without a data offset continues where the previous run ended.
        let mut cursor = None;

        for (kind, run) in Boxes::new(traf) {
            if &kind == b"trun" {
                self.push_run(run, &header, fragment, &mut cursor)?;
            }
        }

        Ok(())
    }

    /// Brings the timeline to the absolute decode time a fragment declares.
    ///
    /// Sample tables describe durations rather than absolute times, so a gap
    /// between fragments is carried by the sample before it.
    fn align(&mut self, base: u64) -> Result<(), MuxError> {
        let Some(origin) = self.origin else {
            self.origin = Some(base);
            return Ok(());
        };

        match base.cmp(&origin.saturating_add(self.decoded)) {
            Ordering::Equal => Ok(()),
            Ordering::Greater => {
                let gap = base - origin.saturating_add(self.decoded);
                let gap = u32::try_from(gap).map_err(|_| MuxError::TooLarge)?;

                let last = self
                    .samples
                    .last_mut()
                    .ok_or(MuxError::Malformed("a fragment precedes every sample"))?;

                last.duration = last.duration.checked_add(gap).ok_or(MuxError::TooLarge)?;
                self.decoded += u64::from(gap);

                Ok(())
            }
            // Fragments of one track never overlap; treating them as if they
            // did would silently drop or duplicate media.
            Ordering::Less => Err(MuxError::Malformed(
                "two fragments cover the same decode time",
            )),
        }
    }

    fn push_run(
        &mut self,
        run: &[u8],
        header: &TrackHeader,
        fragment: u64,
        cursor: &mut Option<u64>,
    ) -> Result<(), MuxError> {
        let mut reader = Reader::new(run);
        let (version, flags) = reader.full()?;
        let count = reader.u32()?;

        let data_offset = if flags & 0x000001 != 0 {
            Some(reader.i32()?)
        } else {
            None
        };

        let first_sample_flags = if flags & 0x000004 != 0 {
            Some(reader.u32()?)
        } else {
            None
        };

        // A run's data offset is relative to the fragment it belongs to, which
        // is what every encoder this muxer meets writes.
        let base = header.base_data_offset.unwrap_or(fragment);

        let mut offset = match data_offset {
            Some(data_offset) => base
                .checked_add_signed(i64::from(data_offset))
                .ok_or(MuxError::Malformed("a run starts outside its file"))?,
            None => cursor.ok_or(MuxError::Malformed("a run has nowhere to start"))?,
        };

        for index in 0..count {
            let duration = if flags & 0x000100 != 0 {
                reader.u32()?
            } else {
                header
                    .default_sample_duration
                    .ok_or(MuxError::Unsupported("a sample declares no duration"))?
            };

            let size = if flags & 0x000200 != 0 {
                reader.u32()?
            } else {
                header
                    .default_sample_size
                    .ok_or(MuxError::Unsupported("a sample declares no size"))?
            };

            let sample_flags = if flags & 0x000400 != 0 {
                Some(reader.u32()?)
            } else if index == 0 {
                first_sample_flags.or(header.default_sample_flags)
            } else {
                header.default_sample_flags
            };

            // Version zero holds an unsigned offset, the signed form is version
            // one; positive offsets are what both forms agree on.
            let composition_offset = if flags & 0x000800 != 0 {
                if version == 0 {
                    reader.u32()? as i32
                } else {
                    reader.i32()?
                }
            } else {
                0
            };

            self.samples.push(Sample {
                offset,
                size,
                duration,
                composition_offset,
                sync: sample_flags.is_none_or(|flags| flags & 0x0001_0000 == 0),
            });

            self.decoded = self
                .decoded
                .checked_add(u64::from(duration))
                .ok_or(MuxError::TooLarge)?;
            offset = offset
                .checked_add(u64::from(size))
                .ok_or(MuxError::TooLarge)?;
        }

        *cursor = Some(offset);

        Ok(())
    }
}

/// The parts of a fragment's track header that describe how samples are stored.
struct TrackHeader {
    base_data_offset: Option<u64>,
    sample_description_index: Option<u32>,
    default_sample_duration: Option<u32>,
    default_sample_size: Option<u32>,
    default_sample_flags: Option<u32>,
}

impl TrackHeader {
    fn read(body: &[u8]) -> Result<Self, MuxError> {
        let mut reader = Reader::new(body);
        let (_version, flags) = reader.full()?;
        let _track_id = reader.u32()?;

        let mut header = Self {
            base_data_offset: None,
            sample_description_index: None,
            default_sample_duration: None,
            default_sample_size: None,
            default_sample_flags: None,
        };

        // The fields follow the flags, in this order.
        if flags & 0x000001 != 0 {
            header.base_data_offset = Some(reader.u64()?);
        }

        if flags & 0x000002 != 0 {
            header.sample_description_index = Some(reader.u32()?);
        }

        if flags & 0x000008 != 0 {
            header.default_sample_duration = Some(reader.u32()?);
        }

        if flags & 0x000010 != 0 {
            header.default_sample_size = Some(reader.u32()?);
        }

        if flags & 0x000020 != 0 {
            header.default_sample_flags = Some(reader.u32()?);
        }

        Ok(header)
    }
}

/// A cursor over a box's payload.
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn u8(&mut self) -> Result<u8, MuxError> {
        let value = *self
            .bytes
            .get(self.position)
            .ok_or(MuxError::Malformed("a box is shorter than it claims"))?;

        self.position += 1;

        Ok(value)
    }

    fn u32(&mut self) -> Result<u32, MuxError> {
        let value = u32::from_be_bytes(self.take(4)?.try_into().expect("four bytes"));

        Ok(value)
    }

    fn i32(&mut self) -> Result<i32, MuxError> {
        let value = i32::from_be_bytes(self.take(4)?.try_into().expect("four bytes"));

        Ok(value)
    }

    fn u64(&mut self) -> Result<u64, MuxError> {
        let value = u64::from_be_bytes(self.take(8)?.try_into().expect("eight bytes"));

        Ok(value)
    }

    /// Reads the version and flags every complete box begins with.
    fn full(&mut self) -> Result<(u8, u32), MuxError> {
        let version = self.u8()?;
        let flags = u32::from_be_bytes([0, self.u8()?, self.u8()?, self.u8()?]);

        Ok((version, flags))
    }

    fn skip(&mut self, count: usize) -> Result<(), MuxError> {
        self.take(count)?;

        Ok(())
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], MuxError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(MuxError::Malformed("a box is longer than it claims"))?;

        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(MuxError::Malformed("a box is shorter than it claims"))?;

        self.position = end;

        Ok(bytes)
    }
}

/// Reads the timescale a media header declares.
fn read_timescale(body: &[u8]) -> Option<u32> {
    let mut reader = Reader::new(body);
    let (version, _flags) = reader.full().ok()?;

    // Creation and modification times come first, and are of no use here.
    reader.skip(if version == 1 { 16 } else { 8 }).ok()?;

    reader.u32().ok()
}

/// Reads the absolute decode time a fragment declares.
fn read_decode_time(body: &[u8]) -> Option<u64> {
    let mut reader = Reader::new(body);
    let (version, _flags) = reader.full().ok()?;

    if version == 1 {
        reader.u64().ok()
    } else {
        reader.u32().ok().map(u64::from)
    }
}

/// A `tkhd` field this muxer replaces.
enum TrackHeaderField {
    Identifier,
    Duration,
}

/// Where a track header keeps a field, which depends on its version.
fn track_header_field(version: u8, field: TrackHeaderField) -> (usize, usize) {
    match (version == 1, field) {
        (false, TrackHeaderField::Identifier) => (12, 4),
        (false, TrackHeaderField::Duration) => (20, 4),
        (true, TrackHeaderField::Identifier) => (20, 4),
        (true, TrackHeaderField::Duration) => (28, 8),
    }
}

/// Where a media header keeps a field, which depends on its version.
fn media_header_field(version: u8) -> (usize, usize) {
    match version {
        1 => (24, 8),
        _ => (16, 4),
    }
}

/// Overwrites a 32-bit field inside a box payload.
fn put_u32(body: &mut [u8], (offset, _width): (usize, usize), value: u32) -> Result<(), MuxError> {
    let field = body
        .get_mut(offset..offset.saturating_add(4))
        .ok_or(MuxError::Malformed("a header is too short for its fields"))?;

    field.copy_from_slice(&value.to_be_bytes());

    Ok(())
}

/// Overwrites a duration inside a box payload.
fn put_duration(
    body: &mut [u8],
    (offset, width): (usize, usize),
    value: u64,
) -> Result<(), MuxError> {
    let field = body
        .get_mut(offset..offset.saturating_add(width))
        .ok_or(MuxError::Malformed("a header is too short for its fields"))?;

    match width {
        8 => field.copy_from_slice(&value.to_be_bytes()),
        _ => {
            let value = u32::try_from(value).map_err(|_| MuxError::TooLarge)?;
            field.copy_from_slice(&value.to_be_bytes());
        }
    }

    Ok(())
}

/// Writes the tracks as one plain MP4.
///
/// Each track's samples are written in one run, so a single chunk covers them
/// and one chunk offset locates them.
async fn write_mp4_layout(
    output: &Path,
    tracks: &[Track],
    force_large_offsets: bool,
) -> Result<u64, MuxError> {
    const MOVIE_TIMESCALE: u32 = 1000;

    let movie_duration = tracks
        .iter()
        .map(|track| track.demuxed.duration_in(MOVIE_TIMESCALE))
        .max()
        .unwrap_or(0);

    // The tables are built before the layout is known, so each chunk offset is
    // written as a placeholder and filled in once the size of the
    // initialization data decides where the sample data starts.
    let ftyp = build_ftyp();
    let payload: u64 = tracks
        .iter()
        .map(|track| track.demuxed.sample_bytes())
        .sum();
    let mut patch = Vec::new();
    let compact = build_moov(tracks, MOVIE_TIMESCALE, movie_duration, &mut patch, false)?;
    let compact_header =
        u64::try_from(ftyp.len() + compact.len() + 8).map_err(|_| MuxError::TooLarge)?;
    let large_offsets =
        force_large_offsets || compact_header.saturating_add(payload) > u64::from(u32::MAX);

    let (moov, patch) = if large_offsets {
        let mut patch = Vec::new();
        let moov = build_moov(tracks, MOVIE_TIMESCALE, movie_duration, &mut patch, true)?;
        (moov, patch)
    } else {
        (compact, patch)
    };
    let mdat_header = mdat_header_size(payload);
    let header =
        u64::try_from(ftyp.len() + moov.len() + mdat_header).map_err(|_| MuxError::TooLarge)?;
    let total = header.checked_add(payload).ok_or(MuxError::TooLarge)?;

    let mut starts = Vec::new();
    let mut start = header;

    for track in tracks {
        starts.push(start);
        start = start
            .checked_add(track.demuxed.sample_bytes())
            .ok_or(MuxError::TooLarge)?;
    }

    let mut moov = moov;

    for (position, start) in patch.iter().zip(&starts) {
        if large_offsets {
            let field = moov
                .get_mut(*position..position.saturating_add(8))
                .ok_or(MuxError::Malformed("a chunk offset was not written"))?;
            field.copy_from_slice(&start.to_be_bytes());
        } else {
            let start = u32::try_from(*start).map_err(|_| MuxError::TooLarge)?;
            let field = moov
                .get_mut(*position..position.saturating_add(4))
                .ok_or(MuxError::Malformed("a chunk offset was not written"))?;
            field.copy_from_slice(&start.to_be_bytes());
        }
    }

    let mut file = BufWriter::new(File::create(output).await.map_err(MuxError::Io)?);
    file.write_all(&ftyp).await.map_err(MuxError::Io)?;
    file.write_all(&moov).await.map_err(MuxError::Io)?;

    if mdat_header == 8 {
        let mdat = u32::try_from(payload + 8).map_err(|_| MuxError::TooLarge)?;
        file.write_all(&mdat.to_be_bytes())
            .await
            .map_err(MuxError::Io)?;
        file.write_all(b"mdat").await.map_err(MuxError::Io)?;
    } else {
        file.write_all(&1_u32.to_be_bytes())
            .await
            .map_err(MuxError::Io)?;
        file.write_all(b"mdat").await.map_err(MuxError::Io)?;
        let size = payload.checked_add(16).ok_or(MuxError::TooLarge)?;
        file.write_all(&size.to_be_bytes())
            .await
            .map_err(MuxError::Io)?;
    }

    let mut buffer = vec![0_u8; COPY_BUFFER];

    for track in tracks {
        copy_samples(&track.path, &track.demuxed.samples, &mut file, &mut buffer).await?;
    }

    file.flush().await.map_err(MuxError::Io)?;
    file.into_inner().sync_all().await.map_err(MuxError::Io)?;

    Ok(total)
}

fn mdat_header_size(payload: u64) -> usize {
    if payload > u64::from(u32::MAX) - 8 {
        16
    } else {
        8
    }
}

/// Copies one track's sample data, in order, into the output.
async fn copy_samples(
    source: &Path,
    samples: &[Sample],
    output: &mut BufWriter<File>,
    buffer: &mut [u8],
) -> Result<(), MuxError> {
    let mut file = File::open(source).await.map_err(MuxError::Io)?;
    let mut position = 0_u64;

    for sample in samples {
        if sample.offset != position {
            file.seek(SeekFrom::Start(sample.offset))
                .await
                .map_err(MuxError::Io)?;
            position = sample.offset;
        }

        let mut remaining = u64::from(sample.size);

        while remaining > 0 {
            let wanted = usize::try_from(remaining.min(COPY_BUFFER as u64)).unwrap_or(COPY_BUFFER);
            let read = file
                .read(&mut buffer[..wanted])
                .await
                .map_err(MuxError::Io)?;

            if read == 0 {
                return Err(MuxError::Malformed(
                    "a sample runs past the end of its track",
                ));
            }

            output
                .write_all(&buffer[..read])
                .await
                .map_err(MuxError::Io)?;

            remaining -= read as u64;
            position += read as u64;
        }
    }

    Ok(())
}

/// Builds the movie box, remembering where each chunk offset has to be filled
/// in.
fn build_moov(
    tracks: &[Track],
    movie_timescale: u32,
    movie_duration: u64,
    patch: &mut Vec<usize>,
    large_offsets: bool,
) -> Result<Vec<u8>, MuxError> {
    let mut writer = Writer::default();

    writer.container(b"moov", |writer| {
        write_mvhd(writer, movie_timescale, movie_duration, tracks.len())?;

        for (index, track) in tracks.iter().enumerate() {
            write_trak(writer, track, index, movie_timescale, patch, large_offsets)?;
        }

        Ok(())
    })?;

    Ok(writer.bytes)
}

/// Writes the movie header.
///
/// Nothing is copied from the inputs: a file that holds tracks from two of them
/// needs a header of its own, and it is short enough to write by hand.
fn write_mvhd(
    writer: &mut Writer,
    timescale: u32,
    duration: u64,
    track_count: usize,
) -> Result<(), MuxError> {
    let mut body = Vec::with_capacity(120);

    body.push(0); // version
    body.extend_from_slice(&[0, 0, 0]); // flags
    body.extend_from_slice(&0_u32.to_be_bytes()); // creation time
    body.extend_from_slice(&0_u32.to_be_bytes()); // modification time
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(
        &u32::try_from(duration)
            .map_err(|_| MuxError::TooLarge)?
            .to_be_bytes(),
    );
    body.extend_from_slice(&0x0001_0000_u32.to_be_bytes()); // rate, 1.0
    body.extend_from_slice(&0x0100_u16.to_be_bytes()); // volume, 1.0
    body.extend_from_slice(&[0, 0]); // reserved
    body.extend_from_slice(&[0; 8]); // reserved
    body.extend_from_slice(&IDENTITY_MATRIX);
    body.extend_from_slice(&[0; 24]); // pre_defined
    body.extend_from_slice(
        &u32::try_from(track_count.saturating_add(1))
            .map_err(|_| MuxError::TooLarge)?
            .to_be_bytes(),
    );

    writer.full(b"mvhd", &body);

    Ok(())
}

/// Writes one track: its header, and everything the file needs to find and time
/// its samples.
fn write_trak(
    writer: &mut Writer,
    track: &Track,
    index: usize,
    movie_timescale: u32,
    patch: &mut Vec<usize>,
    large_offsets: bool,
) -> Result<(), MuxError> {
    let demuxed = &track.demuxed;
    let identifier = u32::try_from(index.saturating_add(1)).map_err(|_| MuxError::TooLarge)?;

    let mut tkhd = demuxed.tkhd.clone();
    let version = tkhd.first().copied().unwrap_or(0);
    put_u32(
        &mut tkhd,
        track_header_field(version, TrackHeaderField::Identifier),
        identifier,
    )?;
    put_duration(
        &mut tkhd,
        track_header_field(version, TrackHeaderField::Duration),
        demuxed.duration_in(movie_timescale),
    )?;

    let mut mdhd = demuxed.mdhd.clone();
    let version = mdhd.first().copied().unwrap_or(0);
    put_duration(&mut mdhd, media_header_field(version), demuxed.duration)?;

    let stts = build_stts(&demuxed.samples)?;
    let ctts = build_ctts(&demuxed.samples)?;
    let stsc = build_stsc(&demuxed.samples)?;
    let stsz = build_stsz(&demuxed.samples)?;
    let stss = build_stss(&demuxed.samples)?;

    writer.container(b"trak", |writer| {
        writer.full(b"tkhd", &tkhd);

        writer.container(b"mdia", |writer| {
            writer.full(b"mdhd", &mdhd);
            writer.full(b"hdlr", &demuxed.hdlr);

            writer.container(b"minf", |writer| {
                writer.full(&demuxed.media_header.0, &demuxed.media_header.1);
                writer.full(b"dinf", &demuxed.dinf);

                writer.container(b"stbl", |writer| {
                    writer.full(b"stsd", &demuxed.stsd);
                    writer.full(b"stts", &stts);

                    if let Some(ctts) = &ctts {
                        writer.full(b"ctts", ctts);
                    }

                    writer.full(b"stsc", &stsc);
                    writer.full(b"stsz", &stsz);

                    if let Some(stss) = &stss {
                        writer.full(b"stss", stss);
                    }

                    write_stco(writer, patch, large_offsets);

                    Ok(())
                })?;

                Ok(())
            })?;

            Ok(())
        })?;

        Ok(())
    })?;

    Ok(())
}

/// Builds the file type of the output.
///
/// `isom` is the base brand every reader knows, and the compatible brands name
/// what the file is made of rather than which codec it happens to hold: a brand
/// is a claim about the container, not about the samples inside it.
fn build_ftyp() -> Vec<u8> {
    let mut writer = Writer::default();
    let position = writer.open(b"ftyp");

    writer.bytes.extend_from_slice(b"isom");
    writer
        .bytes
        .extend_from_slice(&0x0000_0200_u32.to_be_bytes());

    for brand in [b"isom", b"iso2", b"mp41", b"mp42"] {
        writer.bytes.extend_from_slice(brand);
    }

    writer.close(position);

    writer.bytes
}

/// Builds the time-to-sample table.
///
/// One entry per run of equal durations, so a stream whose samples all last the
/// same is one entry long.
fn build_stts(samples: &[Sample]) -> Result<Vec<u8>, MuxError> {
    let mut runs: Vec<(u32, u32)> = Vec::new();

    for sample in samples {
        match runs.last_mut() {
            Some((count, duration)) if *duration == sample.duration => {
                *count = count.checked_add(1).ok_or(MuxError::TooLarge)?;
            }
            _ => runs.push((1, sample.duration)),
        }
    }

    let mut body = vec![0, 0, 0, 0];
    body.extend_from_slice(
        &u32::try_from(runs.len())
            .map_err(|_| MuxError::TooLarge)?
            .to_be_bytes(),
    );

    for (count, duration) in runs {
        body.extend_from_slice(&count.to_be_bytes());
        body.extend_from_slice(&duration.to_be_bytes());
    }

    Ok(body)
}

/// Builds the composition offset table, when the track is reordered.
fn build_ctts(samples: &[Sample]) -> Result<Option<Vec<u8>>, MuxError> {
    if samples.iter().all(|sample| sample.composition_offset == 0) {
        return Ok(None);
    }

    // Version one is the signed form, which a negative offset needs.
    let version = if samples.iter().any(|sample| sample.composition_offset < 0) {
        1
    } else {
        0
    };

    let mut runs: Vec<(u32, i32)> = Vec::new();

    for sample in samples {
        match runs.last_mut() {
            Some((count, offset)) if *offset == sample.composition_offset => {
                *count = count.checked_add(1).ok_or(MuxError::TooLarge)?;
            }
            _ => runs.push((1, sample.composition_offset)),
        }
    }

    let mut body = vec![version, 0, 0, 0];
    body.extend_from_slice(
        &u32::try_from(runs.len())
            .map_err(|_| MuxError::TooLarge)?
            .to_be_bytes(),
    );

    for (count, offset) in runs {
        body.extend_from_slice(&count.to_be_bytes());
        body.extend_from_slice(&offset.to_be_bytes());
    }

    Ok(Some(body))
}

/// Builds the sample-to-chunk table.
///
/// A track is written as one chunk, so one entry describes all of it.
fn build_stsc(samples: &[Sample]) -> Result<Vec<u8>, MuxError> {
    let mut body = vec![0, 0, 0, 0];

    body.extend_from_slice(&1_u32.to_be_bytes()); // one entry
    body.extend_from_slice(&1_u32.to_be_bytes()); // from the first chunk
    body.extend_from_slice(
        &u32::try_from(samples.len())
            .map_err(|_| MuxError::TooLarge)?
            .to_be_bytes(),
    );
    body.extend_from_slice(&1_u32.to_be_bytes()); // the only sample entry

    Ok(body)
}

/// Builds the sample size table.
fn build_stsz(samples: &[Sample]) -> Result<Vec<u8>, MuxError> {
    let mut body = vec![0, 0, 0, 0]; // sizes are not uniform
    body.extend_from_slice(&0_u32.to_be_bytes());
    body.extend_from_slice(
        &u32::try_from(samples.len())
            .map_err(|_| MuxError::TooLarge)?
            .to_be_bytes(),
    );

    for sample in samples {
        body.extend_from_slice(&sample.size.to_be_bytes());
    }

    Ok(body)
}

/// Builds the sync sample table, when not every sample is one.
///
/// A table that lists every sample says nothing a reader does not already
/// assume, so it is left out entirely.
fn build_stss(samples: &[Sample]) -> Result<Option<Vec<u8>>, MuxError> {
    if samples.iter().all(|sample| sample.sync) {
        return Ok(None);
    }

    let mut numbers = Vec::new();

    for (index, sample) in samples.iter().enumerate() {
        if sample.sync {
            numbers.push(u32::try_from(index.saturating_add(1)).map_err(|_| MuxError::TooLarge)?);
        }
    }

    let mut body = vec![0, 0, 0, 0];
    body.extend_from_slice(
        &u32::try_from(numbers.len())
            .map_err(|_| MuxError::TooLarge)?
            .to_be_bytes(),
    );

    for number in numbers {
        body.extend_from_slice(&number.to_be_bytes());
    }

    Ok(Some(body))
}

/// Writes the chunk offset table, leaving the offset for the file layout to
/// fill in: the initialization data has to be measured first to know it.
fn write_stco(writer: &mut Writer, patch: &mut Vec<usize>, large_offsets: bool) {
    let kind = if large_offsets { b"co64" } else { b"stco" };
    let position = writer.open(kind);

    writer.bytes.extend_from_slice(&[0, 0, 0, 0]); // version and flags
    writer.bytes.extend_from_slice(&1_u32.to_be_bytes()); // one chunk
    patch.push(writer.bytes.len());
    writer
        .bytes
        .extend_from_slice(if large_offsets { &[0; 8] } else { &[0; 4] }); // filled in later

    writer.close(position);
}

/// Builds a box tree into one buffer.
///
/// A box's size can only be known once its contents are written, so a box is
/// opened before its children and closed after them.
#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// Starts a box and returns where its size field was written.
    fn open(&mut self, kind: &[u8; 4]) -> usize {
        let position = self.bytes.len();

        self.bytes.extend_from_slice(&[0, 0, 0, 0]);
        self.bytes.extend_from_slice(kind);

        position
    }

    /// Finishes the box that began at `position`.
    fn close(&mut self, position: usize) {
        let length = u32::try_from(self.bytes.len().saturating_sub(position))
            .unwrap_or(u32::MAX)
            .to_be_bytes();

        self.bytes[position..position.saturating_add(4)].copy_from_slice(&length);
    }

    /// Writes a box whose payload is already at hand.
    fn full(&mut self, kind: &[u8; 4], body: &[u8]) {
        let position = self.open(kind);

        self.bytes.extend_from_slice(body);
        self.close(position);
    }

    /// Writes a box whose payload is built in place.
    fn container<T>(
        &mut self,
        kind: &[u8; 4],
        build: impl FnOnce(&mut Self) -> Result<T, MuxError>,
    ) -> Result<T, MuxError> {
        let position = self.open(kind);
        let value = build(self)?;

        self.close(position);

        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{Boxes, MuxError, find_box, mux_mp4_with_layout};

    /// The track identifier a fixture's fragments refer to.
    const TRACK_ID: u32 = 1;

    /// One sample of a fixture track.
    #[derive(Clone, Copy)]
    struct Spec {
        /// The byte every byte of the sample's data is set to, so that the
        /// output can be checked without knowing what the sample is.
        byte: u8,
        size: u32,
        duration: u32,
        composition_offset: i32,
        sync: bool,
    }

    /// A sample that plays on its own and is not reordered.
    const fn sample(byte: u8, size: u32, duration: u32) -> Spec {
        Spec {
            byte,
            size,
            duration,
            composition_offset: 0,
            sync: true,
        }
    }

    /// A sample that depends on others, the way a compressed video frame does.
    const fn dependent(byte: u8, size: u32, duration: u32, composition_offset: i32) -> Spec {
        Spec {
            byte,
            size,
            duration,
            composition_offset,
            sync: false,
        }
    }

    /// What one fragment of a fixture carries.
    struct Fragment {
        /// The group that the encode was segmented into, in the track's
        /// timescale; the samples of one group decode one after another.
        decode_time: u64,
        samples: Vec<Spec>,
    }

    /// Muxes two fixture tracks and returns the bytes of the result.
    async fn mux(video: &[u8], audio: &[u8]) -> Vec<u8> {
        mux_with_layout(video, audio, false).await
    }

    async fn mux_with_layout(video: &[u8], audio: &[u8], force_large_offsets: bool) -> Vec<u8> {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let video_path = directory.path().join("video.track");
        let audio_path = directory.path().join("audio.track");
        let output = directory.path().join("muxed.mp4");

        std::fs::write(&video_path, video).expect("the fixture must be written");
        std::fs::write(&audio_path, audio).expect("the fixture must be written");

        let written = mux_mp4_with_layout(&video_path, &audio_path, &output, force_large_offsets)
            .await
            .expect("muxing must succeed");

        let bytes = std::fs::read(&output).expect("the output must be readable");

        assert_eq!(
            written,
            bytes.len() as u64,
            "the reported size is the file's size"
        );

        bytes
    }

    /// Muxes fixtures that are expected to be refused.
    async fn mux_refused(video: &[u8], audio: &[u8]) -> MuxError {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let video_path = directory.path().join("video.track");
        let audio_path = directory.path().join("audio.track");
        let output = directory.path().join("muxed.mp4");

        std::fs::write(&video_path, video).expect("the fixture must be written");
        std::fs::write(&audio_path, audio).expect("the fixture must be written");

        mux_mp4_with_layout(&video_path, &audio_path, &output, false)
            .await
            .expect_err("muxing must be refused")
    }

    // The fixtures below build the fragmented MP4 that a segmenter produces.
    // They deliberately know nothing about how the muxer is written: they are
    // the input a real origin serves, reduced to what the format requires.

    /// One complete box, carrying its payload.
    fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut payload = vec![version];
        payload.extend_from_slice(&flags.to_be_bytes()[1..]);
        payload.extend_from_slice(body);

        boxed(kind, &payload)
    }

    /// One container box, carrying boxes.
    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();

        out.extend_from_slice(
            &u32::try_from(body.len().saturating_add(8))
                .expect("a fixture box fits in a file")
                .to_be_bytes(),
        );
        out.extend_from_slice(kind);
        out.extend_from_slice(body);

        out
    }

    /// A track header, as an initialization segment carries it.
    fn tkhd(width: u16, height: u16) -> Vec<u8> {
        let mut body = vec![0; 8]; // creation and modification time
        body.extend_from_slice(&TRACK_ID.to_be_bytes());
        body.extend_from_slice(&0_u32.to_be_bytes()); // reserved
        body.extend_from_slice(&0_u32.to_be_bytes()); // duration
        body.extend_from_slice(&[0; 8]); // reserved
        body.extend_from_slice(&[0; 2]); // layer
        body.extend_from_slice(&[0; 2]); // alternate group
        body.extend_from_slice(&0x0100_u16.to_be_bytes()); // volume
        body.extend_from_slice(&[0; 2]); // reserved
        body.extend_from_slice(&super::IDENTITY_MATRIX);
        body.extend_from_slice(&(u32::from(width) << 16).to_be_bytes());
        body.extend_from_slice(&(u32::from(height) << 16).to_be_bytes());

        full(b"tkhd", 0, 7, &body)
    }

    /// A media header, as an initialization segment carries it.
    fn mdhd(timescale: u32) -> Vec<u8> {
        let mut body = vec![0; 8]; // creation and modification time
        body.extend_from_slice(&timescale.to_be_bytes());
        body.extend_from_slice(&0_u32.to_be_bytes()); // duration
        body.extend_from_slice(&0x55c4_u16.to_be_bytes()); // language, "und"
        body.extend_from_slice(&0_u16.to_be_bytes()); // pre_defined

        full(b"mdhd", 0, 0, &body)
    }

    /// A handler, which is what tells a reader what the track holds.
    fn hdlr(handler: &[u8; 4]) -> Vec<u8> {
        let mut body = vec![0; 4]; // pre_defined
        body.extend_from_slice(handler);
        body.extend_from_slice(&[0; 12]); // reserved
        body.extend_from_slice(b"fixture\0");

        full(b"hdlr", 0, 0, &body)
    }

    /// A data reference, which an MP4 describes even when it refers to itself.
    fn dinf() -> Vec<u8> {
        boxed(b"dinf", &full(b"dref", 0, 0, &[0, 0, 0, 1, 0, 0, 0, 1]))
    }

    /// The sample entry of an H.264 track.
    fn avc1(width: u16, height: u16) -> Vec<u8> {
        let mut body = vec![0; 6]; // reserved
        body.extend_from_slice(&1_u16.to_be_bytes()); // data reference index
        body.extend_from_slice(&[0; 16]); // pre_defined and reserved
        body.extend_from_slice(&width.to_be_bytes());
        body.extend_from_slice(&height.to_be_bytes());
        body.extend_from_slice(&0x0048_0000_u32.to_be_bytes()); // 72 dpi
        body.extend_from_slice(&0x0048_0000_u32.to_be_bytes());
        body.extend_from_slice(&0_u32.to_be_bytes()); // reserved
        body.extend_from_slice(&1_u16.to_be_bytes()); // frame count
        body.extend_from_slice(&[0; 32]); // compressor name
        body.extend_from_slice(&0x0018_u16.to_be_bytes()); // depth
        body.extend_from_slice(&0xffff_u16.to_be_bytes()); // pre_defined

        // A configuration record carrying parameter sets. The bytes are a
        // fixture, not an encode: what matters is that they travel untouched.
        let parameter_sets = [
            0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x00, 0xf0, 0x11, 0x7e, 0xf0, 0x11, 0x00, 0x00, 0x03,
            0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x32, 0x0f, 0x12, 0x26, 0xa0,
        ];
        let picture_parameters = [0x68, 0xeb, 0xec, 0xb2, 0x2c];

        let mut record = vec![0x01, 0x42, 0xc0, 0x1e, 0xff, 0xe1];
        record.extend_from_slice(&(parameter_sets.len() as u16).to_be_bytes());
        record.extend_from_slice(&parameter_sets);
        record.push(1);
        record.extend_from_slice(&(picture_parameters.len() as u16).to_be_bytes());
        record.extend_from_slice(&picture_parameters);

        body.extend_from_slice(&boxed(b"avcC", &record));

        boxed(b"avc1", &body)
    }

    /// The sample entry of an AAC track.
    fn mp4a() -> Vec<u8> {
        let mut body = vec![0; 6]; // reserved
        body.extend_from_slice(&1_u16.to_be_bytes()); // data reference index
        body.extend_from_slice(&[0; 8]); // reserved
        body.extend_from_slice(&2_u16.to_be_bytes()); // channel count
        body.extend_from_slice(&16_u16.to_be_bytes()); // sample size
        body.extend_from_slice(&[0; 4]); // pre_defined and reserved
        body.extend_from_slice(&0xac44_0000_u32.to_be_bytes()); // 44100 Hz

        // AAC-LC, 44.1 kHz, stereo, as a decoder configuration.
        let decoder = [
            0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x05, 0x02, 0x12, 0x10,
        ];
        let mut es = vec![0, 0, 0];
        es.extend_from_slice(&[0x04, 0x11]);
        es.extend_from_slice(&decoder);
        es.extend_from_slice(&[0x06, 0x01, 0x02]);

        let mut record = vec![0, 0, 0, 0]; // version and flags
        record.extend_from_slice(&es);

        body.extend_from_slice(&boxed(b"esds", &record));

        boxed(b"mp4a", &body)
    }

    /// An initialization segment, as a segmenter writes it.
    fn init(handler: &[u8; 4], timescale: u32) -> Vec<u8> {
        let (media_header, entry) = if handler == b"vide" {
            (full(b"vmhd", 0, 1, &[0; 8]), avc1(640, 360))
        } else {
            (full(b"smhd", 0, 0, &[0; 4]), mp4a())
        };

        // A fragmented track keeps its tables empty: its samples live in the
        // fragments, and the tables that describe them are built from those.
        let mut stsd = vec![0, 0, 0, 0]; // version and flags
        stsd.extend_from_slice(&1_u32.to_be_bytes()); // one sample entry
        stsd.extend_from_slice(&entry);

        let mut stbl = Vec::new();
        stbl.extend_from_slice(&boxed(b"stsd", &stsd));
        stbl.extend_from_slice(&full(b"stts", 0, 0, &[]));
        stbl.extend_from_slice(&full(b"stsc", 0, 0, &[]));
        stbl.extend_from_slice(&full(b"stsz", 0, 0, &[0, 0, 0, 0, 0, 0, 0, 0]));
        stbl.extend_from_slice(&full(b"stco", 0, 0, &[]));

        let mut minf = media_header;
        minf.extend_from_slice(&dinf());
        minf.extend_from_slice(&boxed(b"stbl", &stbl));

        let mut mdia = mdhd(timescale);
        mdia.extend_from_slice(&hdlr(handler));
        mdia.extend_from_slice(&boxed(b"minf", &minf));

        let mut moov = Vec::new();
        moov.extend_from_slice(&tkhd(640, 360));
        moov.extend_from_slice(&boxed(b"mdia", &mdia));

        let mut file = Vec::new();
        file.extend_from_slice(&boxed(b"ftyp", b"iso5\x00\x00\x02\x00iso5iso6mp41"));
        file.extend_from_slice(&boxed(b"moov", &boxed(b"trak", &moov)));

        file
    }

    /// One fragment: a description of the samples, then the samples.
    fn fragment(fragment: &Fragment) -> Vec<u8> {
        // The data offset is where the samples begin, which is only known once
        // the description is complete, so the description is built twice.
        let mut moof = describe(fragment, 0);
        let offset = i32::try_from(moof.len().saturating_add(8)).expect("a fixture fragment");

        moof = describe(fragment, offset);

        let mut data = Vec::new();

        for sample in &fragment.samples {
            data.extend(std::iter::repeat_n(sample.byte, sample.size as usize));
        }

        moof.extend_from_slice(&boxed(b"mdat", &data));

        moof
    }

    /// The description of one fragment, with its sample data offset.
    fn describe(fragment: &Fragment, data_offset: i32) -> Vec<u8> {
        let mut tfhd = TRACK_ID.to_be_bytes().to_vec();
        let mut traf = full(b"tfhd", 0, 0x020000, &tfhd); // base is the fragment

        let mut tfdt = Vec::new();
        tfdt.extend_from_slice(&fragment.decode_time.to_be_bytes());

        traf.extend_from_slice(&full(b"tfdt", 1, 0, &tfdt));

        let mut run = Vec::new();
        run.extend_from_slice(
            &u32::try_from(fragment.samples.len())
                .expect("a fixture fragment")
                .to_be_bytes(),
        );
        run.extend_from_slice(&data_offset.to_be_bytes());

        for sample in &fragment.samples {
            run.extend_from_slice(&sample.duration.to_be_bytes());
            run.extend_from_slice(&sample.size.to_be_bytes());
            run.extend_from_slice(&flags(sample.sync).to_be_bytes());
            run.extend_from_slice(&sample.composition_offset.to_be_bytes());
        }

        traf.extend_from_slice(&full(
            b"trun",
            1,
            0x0001 | 0x0100 | 0x0200 | 0x0400 | 0x0800,
            &run,
        ));

        tfhd.clear();

        boxed(b"moof", &boxed(b"traf", &traf))
    }

    /// The sample flags that say whether a sample stands on its own.
    fn flags(sync: bool) -> u32 {
        if sync { 0x0200_0000 } else { 0x0101_0000 }
    }

    /// A video track: two groups, the second reordered and re-based.
    fn video_track() -> Vec<u8> {
        let mut file = init(b"vide", 30_000);

        file.extend_from_slice(&fragment(&Fragment {
            decode_time: 0,
            samples: vec![sample(0xa1, 4, 1000), dependent(0xa2, 4, 1000, 3000)],
        }));

        file.extend_from_slice(&fragment(&Fragment {
            decode_time: 2000,
            samples: vec![sample(0xa3, 4, 1000)],
        }));

        file
    }

    /// An audio track: one group of two samples.
    fn audio_track() -> Vec<u8> {
        let mut file = init(b"soun", 44_100);

        file.extend_from_slice(&fragment(&Fragment {
            decode_time: 0,
            samples: vec![sample(0xb1, 8, 1024), sample(0xb2, 8, 1024)],
        }));

        file
    }

    /// Every box of a file, in order, with the path that reaches it.
    fn boxes(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut found = Vec::new();

        collect(bytes, "", &mut found);

        found
    }

    fn collect(bytes: &[u8], path: &str, found: &mut Vec<(String, Vec<u8>)>) {
        for (kind, body) in Boxes::new(bytes) {
            let kind = String::from_utf8_lossy(&kind).into_owned();
            let here = if path.is_empty() {
                kind.clone()
            } else {
                format!("{path}/{kind}")
            };

            found.push((here.clone(), body.to_vec()));

            if matches!(
                kind.as_str(),
                "moov" | "trak" | "mdia" | "minf" | "stbl" | "moof" | "traf" | "dinf"
            ) {
                collect(body, &here, found);
            }
        }
    }

    /// The payload of one box, found by the path that reaches it.
    fn box_at<'a>(found: &'a [(String, Vec<u8>)], path: &str, index: usize) -> &'a [u8] {
        let paths: Vec<&str> = found.iter().map(|(at, _)| at.as_str()).collect();

        found
            .iter()
            .filter(|(at, _)| at == path)
            .nth(index)
            .map(|(_, body)| body.as_slice())
            .unwrap_or_else(|| panic!("no box at {path}[{index}] among {paths:?}"))
    }

    /// The payload of a box that every track carries, one per track.
    fn all_tracks(found: &[(String, Vec<u8>)], name: &str) -> Vec<Vec<u8>> {
        let path = format!("moov/trak/mdia/minf/stbl/{name}");

        found
            .iter()
            .filter(move |(at, _)| at == &path)
            .map(|(_, body)| body.clone())
            .collect()
    }

    /// The payload of one track's box of that name.
    fn track<'a>(found: &'a [(String, Vec<u8>)], index: usize, name: &str) -> &'a [u8] {
        box_at(found, &format!("moov/trak/mdia/minf/stbl/{name}"), index)
    }

    /// The 32-bit values after a complete box's version, flags and count.
    fn values(body: &[u8]) -> Vec<u32> {
        body.get(8..)
            .unwrap_or_default()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|value| u32::from_be_bytes(*value))
            .collect()
    }

    /// The sample sizes of a size table, which carries one field more than the
    /// other tables do.
    fn sizes(body: &[u8]) -> Vec<u32> {
        values(body.get(4..).unwrap_or_default())
    }

    /// The runs of a table that holds (count, value) pairs.
    fn runs(body: &[u8]) -> Vec<(u32, u32)> {
        values(body)
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (pair[0], pair[1]))
            .collect()
    }

    #[tokio::test]
    async fn two_tracks_become_one_file_whose_tables_describe_both() {
        let bytes = mux(&video_track(), &audio_track()).await;
        let found = boxes(&bytes);

        // Two tracks, numbered from one, in the order they were given.
        let video = box_at(&found, "moov/trak/tkhd", 0);
        let audio = box_at(&found, "moov/trak/tkhd", 1);

        assert_eq!(&video[12..16], 1_u32.to_be_bytes());
        assert_eq!(&audio[12..16], 2_u32.to_be_bytes());

        // The movie is as long as its longest track, in the timescale its own
        // header declares.
        let mvhd = box_at(&found, "moov/mvhd", 0);

        assert_eq!(&mvhd[12..16], 1000_u32.to_be_bytes());
        assert_eq!(&mvhd[16..20], 100_u32.to_be_bytes());

        // The video is three samples of 1000 ticks out of 30000, the audio two
        // of 1024 out of 44100.
        assert_eq!(&video[20..24], 100_u32.to_be_bytes());
        assert_eq!(&audio[20..24], 46_u32.to_be_bytes());

        let sizes: Vec<Vec<u32>> = all_tracks(&found, "stsz")
            .iter()
            .map(|body| sizes(body))
            .collect();

        assert_eq!(sizes, [vec![4, 4, 4], vec![8, 8]]);

        // Every sample of each track lasts the same, so one run describes them
        // all.
        assert_eq!(runs(track(&found, 0, "stts")), [(3, 1000)]);
        assert_eq!(runs(track(&found, 1, "stts")), [(2, 1024)]);

        // Only the video track is reordered, so only it carries offsets.
        assert_eq!(runs(track(&found, 0, "ctts")), [(1, 0), (1, 3000), (1, 0)]);
        assert_eq!(all_tracks(&found, "ctts").len(), 1);

        // Only the video track has samples that do not stand on their own, so
        // only it lists the ones that do.
        assert_eq!(values(track(&found, 0, "stss")), [1, 3]);
        assert_eq!(all_tracks(&found, "stss").len(), 1);

        // The chunk offsets point at the sample data, which is the source bytes
        // in the order the tracks came in.
        let video_offset = values(track(&found, 0, "stco"))[0] as usize;
        let audio_offset = values(track(&found, 1, "stco"))[0] as usize;

        assert_eq!(
            &bytes[video_offset..video_offset + 12],
            [
                0xa1, 0xa1, 0xa1, 0xa1, 0xa2, 0xa2, 0xa2, 0xa2, 0xa3, 0xa3, 0xa3, 0xa3
            ]
        );
        assert_eq!(audio_offset, video_offset + 12);
        assert_eq!(
            &bytes[audio_offset..audio_offset + 16],
            [
                0xb1, 0xb1, 0xb1, 0xb1, 0xb1, 0xb1, 0xb1, 0xb1, 0xb2, 0xb2, 0xb2, 0xb2, 0xb2, 0xb2,
                0xb2, 0xb2
            ]
        );

        // Nothing else is copied: the file ends where the last sample does, and
        // the initialization segments keep no samples of their own.
        assert_eq!(bytes.len(), audio_offset + 16);
    }

    #[tokio::test]
    async fn forced_large_layout_writes_64_bit_chunk_offsets() {
        let bytes = mux_with_layout(&video_track(), &audio_track(), true).await;
        let found = boxes(&bytes);
        let offsets = all_tracks(&found, "co64");

        assert_eq!(offsets.len(), 2);
        assert!(all_tracks(&found, "stco").is_empty());

        for table in offsets {
            assert_eq!(&table[4..8], &1_u32.to_be_bytes());
            assert!(u64::from_be_bytes(table[8..16].try_into().unwrap()) >= 8);
        }
    }

    #[test]
    fn extended_mdat_header_is_used_at_the_32_bit_limit() {
        assert_eq!(super::mdat_header_size(u64::from(u32::MAX) - 8), 8);
        assert_eq!(super::mdat_header_size(u64::from(u32::MAX) - 7), 16);
    }

    #[tokio::test]
    async fn the_sample_entry_of_each_track_is_carried_over_unchanged() {
        let video = video_track();
        let audio = audio_track();

        let source = |track: &[u8]| {
            find_box(
                find_box(track, &[b"moov", b"trak"]).expect("a track"),
                &[b"mdia", b"minf", b"stbl", b"stsd"],
            )
            .expect("a sample entry")
            .to_vec()
        };

        let (source_video, source_audio) = (source(&video), source(&audio));
        let bytes = mux(&video, &audio).await;
        let found = boxes(&bytes);

        assert_eq!(
            all_tracks(&found, "stsd"),
            [source_video, source_audio],
            "each track describes itself exactly as the origin did"
        );
    }

    #[tokio::test]
    async fn a_gap_between_fragments_lengthens_the_sample_before_it() {
        let mut video = init(b"vide", 30_000);

        video.extend_from_slice(&fragment(&Fragment {
            decode_time: 0,
            samples: vec![sample(0xa1, 4, 1000)],
        }));

        // The second fragment decodes later than the first ended, which is a
        // gap the sample before it has to carry.
        video.extend_from_slice(&fragment(&Fragment {
            decode_time: 3000,
            samples: vec![sample(0xa2, 4, 1000)],
        }));

        let bytes = mux(&video, &audio_track()).await;
        let found = boxes(&bytes);

        assert_eq!(runs(track(&found, 0, "stts")), [(1, 3000), (1, 1000)]);
        assert_eq!(
            &box_at(&found, "moov/trak/mdia/mdhd", 0)[16..20],
            4000_u32.to_be_bytes()
        );
    }

    #[tokio::test]
    async fn a_track_without_an_initialization_segment_is_refused() {
        let error = mux_refused(
            &fragment(&Fragment {
                decode_time: 0,
                samples: vec![sample(0xa1, 4, 1000)],
            }),
            &audio_track(),
        )
        .await;

        assert!(
            matches!(error, MuxError::Malformed(what) if what.contains("initialization")),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn a_sample_that_runs_past_the_end_of_its_track_is_refused() {
        let mut video = init(b"vide", 30_000);

        // The fragment describes more data than it carries.
        video.extend_from_slice(&fragment(&Fragment {
            decode_time: 0,
            samples: vec![sample(0xa1, 64, 1000)],
        }));

        let truncated = &video[..video.len() - 32];
        let error = mux_refused(truncated, &audio_track()).await;

        assert!(
            matches!(error, MuxError::Malformed(what) if what.contains("past the end")),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn a_track_that_carries_no_samples_is_refused() {
        let error = mux_refused(&init(b"vide", 30_000), &audio_track()).await;

        assert!(
            matches!(error, MuxError::Malformed(what) if what.contains("no samples")),
            "unexpected error: {error}"
        );
    }
}
