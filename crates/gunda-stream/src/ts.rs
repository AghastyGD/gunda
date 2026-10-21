//! Reading H.264 and AAC samples out of an MPEG-2 transport stream.
//!
//! HLS delivers a presentation either as fragmented MP4 (`EXT-X-MAP` followed
//! by `.m4s`) or as a transport stream (`.ts`). The transport stream is the more
//! common form, and it is not a sequence of files that concatenate into a
//! playable one: every 188-byte packet carries a share of one elementary stream,
//! and the sample boundaries are implicit in the codec's own byte stream. There
//! is no sample table anywhere to carry over.
//!
//! This module turns such a stream back into one elementary track: the sample
//! bytes written back to back into a new file, plus the timing, codec
//! configuration and picture size an ISO base media writer needs. Only H.264
//! video and ADTS-framed AAC audio are supported. Anything else is refused
//! rather than guessed at: a wrong sample table produces a file that opens and
//! plays the wrong thing.
//!
//! What this does not do is validate the media. The samples are copied exactly
//! as the transport stream carried them, and only the framing around them is
//! rewritten.

use std::fs::File;
use std::io::{BufReader, ErrorKind, Read, Write};
use std::path::Path;

use crate::container::id3v2_length;
use crate::mux::MuxError;

/// One transport-stream packet, sync byte included.
const PACKET: usize = 188;

/// The byte every packet starts with.
const SYNC: u8 = 0x47;

/// A program table larger than this is not a program table.
const MAX_SECTION: usize = 4 * 1024;

/// The largest elementary-stream backlog this keeps while waiting for the rest
/// of a NAL unit or an ADTS frame.
const MAX_PENDING: usize = 8 * 1024 * 1024;

/// The clock transport-stream timestamps are expressed in.
const VIDEO_TIMESCALE: u32 = 90_000;

/// Duration assumed for a video sample whose neighbours cannot tell.
///
/// One frame at 30 frames per second, in 90 kHz ticks. Only a track holding a
/// single sample ever uses it.
const DEFAULT_FRAME_DURATION: u32 = VIDEO_TIMESCALE / 30;

/// The longest duration one video sample may claim.
///
/// Timestamps restart across a discontinuity, and a jump in the stream must not
/// turn into a sample that waits minutes before the next one. One second is
/// longer than any real frame interval in the streams this reads.
const MAX_FRAME_DURATION: u32 = VIDEO_TIMESCALE;

/// Samples per AAC frame, which is what one ADTS frame holds.
const AAC_FRAME_SAMPLES: u32 = 1024;

/// Sample rates an ADTS header can name, by frequency index.
const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// One sample of an elementary track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ElementarySample {
    /// Offset of the sample's first byte in the elementary file.
    pub(crate) offset: u64,
    pub(crate) size: u32,
    /// Decode duration, in the track's timescale.
    pub(crate) duration: u32,
    /// Presentation time minus decode time, in the track's timescale.
    pub(crate) composition_offset: i32,
    pub(crate) sync: bool,
}

/// The codec configuration a sample entry has to carry.
#[derive(Debug)]
pub(crate) enum Codec {
    /// H.264, with the contents of an `avcC` box already assembled.
    Avc {
        configuration: Vec<u8>,
        width: u16,
        height: u16,
    },
    /// AAC, with the AudioSpecificConfig an `esds` box carries.
    Aac {
        configuration: u16,
        channel_count: u16,
        sample_rate: u32,
    },
}

/// One track read out of a transport stream, and where its bytes now are.
#[derive(Debug)]
pub(crate) struct ElementaryTrack {
    pub(crate) codec: Codec,
    pub(crate) timescale: u32,
    pub(crate) samples: Vec<ElementarySample>,
    /// Decode time just after the last sample, in `timescale`.
    pub(crate) duration: u64,
}

/// Reads the one track a transport stream carries into an elementary file.
///
/// `output` is created or truncated: it receives the sample bytes in order, so
/// every sample offset is contiguous. A stream carrying both a picture and a
/// sound is refused rather than silently reduced to one of them; the caller
/// decides which track a file is supposed to hold.
pub(crate) fn demux(source: &Path, output: &Path) -> Result<ElementaryTrack, MuxError> {
    let file = File::open(source).map_err(MuxError::Io)?;
    let mut reader = BufReader::with_capacity(PACKET * 32, file);
    let mut out = File::create(output).map_err(MuxError::Io)?;
    let mut demuxer = Demuxer::new(&mut out);

    let mut packet = [0_u8; PACKET];
    let mut started = false;

    loop {
        match reader.read_exact(&mut packet) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(MuxError::Io(error)),
        }

        if packet[0] != SYNC {
            return Err(if started {
                MuxError::Malformed("the transport stream stops being packet aligned")
            } else {
                MuxError::Malformed("the transport stream does not begin on a packet boundary")
            });
        }

        started = true;
        demuxer.packet(&packet)?;
    }

    if !started {
        return Err(MuxError::Malformed("the transport stream is empty"));
    }

    demuxer.finish()
}

/// Reads a file that is already ADTS-framed AAC into an elementary file.
///
/// A packed-audio rendition of HLS can be served this way, without the
/// transport stream around it.
pub(crate) fn demux_adts(source: &Path, output: &Path) -> Result<ElementaryTrack, MuxError> {
    let mut input = File::open(source).map_err(MuxError::Io)?;
    let mut out = File::create(output).map_err(MuxError::Io)?;
    let mut audio = AudioTrack::default();
    let mut written = 0_u64;
    let mut samples = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = input.read(&mut buffer).map_err(MuxError::Io)?;
        if read == 0 {
            break;
        }

        let mut frames = Vec::new();
        audio.push(&buffer[..read], &mut frames)?;
        write_adts_frames(&mut out, frames, &mut samples, &mut written)?;
    }

    let mut frames = Vec::new();
    audio.flush(&mut frames);
    write_adts_frames(&mut out, frames, &mut samples, &mut written)?;
    out.flush().map_err(MuxError::Io)?;

    audio.samples = samples;

    audio.into_track()
}

fn write_adts_frames(
    output: &mut File,
    frames: Vec<Vec<u8>>,
    samples: &mut Vec<ElementarySample>,
    written: &mut u64,
) -> Result<(), MuxError> {
    for frame in frames {
        let size = u32::try_from(frame.len()).map_err(|_| MuxError::TooLarge)?;
        output.write_all(&frame).map_err(MuxError::Io)?;
        samples.push(ElementarySample {
            offset: *written,
            size,
            duration: AAC_FRAME_SAMPLES,
            composition_offset: 0,
            sync: true,
        });
        *written = (*written).saturating_add(u64::from(size));
    }

    Ok(())
}

/// The transport stream being read.
struct Demuxer<'a> {
    output: &'a mut File,
    /// How many bytes have been written to the elementary file so far.
    written: u64,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    audio_pid: Option<u16>,
    video: Option<VideoTrack>,
    audio: Option<AudioTrack>,
    unsupported_video: Option<&'static str>,
    unsupported_audio: Option<&'static str>,
    pat: Section,
    pmt: Section,
}

impl<'a> Demuxer<'a> {
    fn new(output: &'a mut File) -> Self {
        Self {
            output,
            written: 0,
            pmt_pid: None,
            video_pid: None,
            audio_pid: None,
            video: None,
            audio: None,
            unsupported_video: None,
            unsupported_audio: None,
            pat: Section::default(),
            pmt: Section::default(),
        }
    }

    /// Reads one 188-byte packet.
    fn packet(&mut self, packet: &[u8]) -> Result<(), MuxError> {
        if packet.len() != PACKET {
            return Err(MuxError::Malformed(
                "a transport-stream packet is not 188 bytes",
            ));
        }

        // A packet the transport flagged as damaged carries no trustworthy
        // payload; skipping it is what a player does too.
        if packet[1] & 0x80 != 0 {
            return Ok(());
        }

        let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
        let unit_start = packet[1] & 0x40 != 0;
        let adaptation = (packet[3] >> 4) & 0x03;
        let mut offset = 4;

        if adaptation & 0x02 != 0 {
            let length = usize::from(packet[4]);
            offset = 5 + length;

            if offset > PACKET {
                return Err(MuxError::Malformed(
                    "a packet's adaptation field runs past its end",
                ));
            }
        }

        // Adaptation without payload: the packet only carries timing or stuffing.
        if adaptation == 0x02 || offset >= PACKET {
            return Ok(());
        }

        let payload = &packet[offset..];

        if pid == 0 {
            return self.programs(unit_start, payload);
        }

        if Some(pid) == self.pmt_pid {
            return self.program_map(unit_start, payload);
        }

        if Some(pid) == self.video_pid {
            return self.video_payload(unit_start, payload);
        }

        if Some(pid) == self.audio_pid {
            return self.audio_payload(unit_start, payload);
        }

        Ok(())
    }

    /// Reads a program association table.
    fn programs(&mut self, start: bool, payload: &[u8]) -> Result<(), MuxError> {
        for section in self.pat.push(payload, start)? {
            if section.first() != Some(&0x00) || section.len() < 12 {
                continue;
            }

            // Three bytes of table header, then the transport stream identifier,
            // version, section number and last section number.
            let mut index = 8;
            let end = section.len().saturating_sub(4);

            while index + 4 <= end {
                let program = u16::from_be_bytes([section[index], section[index + 1]]);
                let pid = u16::from_be_bytes([section[index + 2], section[index + 3]]) & 0x1FFF;

                if program != 0 {
                    self.pmt_pid = Some(pid);
                    return Ok(());
                }

                index += 4;
            }
        }

        Ok(())
    }

    /// Reads a program map table and learns which elementary streams it carries.
    fn program_map(&mut self, start: bool, payload: &[u8]) -> Result<(), MuxError> {
        for section in self.pmt.push(payload, start)? {
            if section.first() != Some(&0x02) || section.len() < 16 {
                continue;
            }

            let program_info = ((usize::from(section[10] & 0x0F)) << 8) | usize::from(section[11]);
            let mut index = 12 + program_info;
            let end = section.len().saturating_sub(4);

            while index + 5 <= end {
                let stream_type = section[index];
                let pid = u16::from_be_bytes([section[index + 1], section[index + 2]]) & 0x1FFF;
                let info = ((usize::from(section[index + 3] & 0x0F)) << 8)
                    | usize::from(section[index + 4]);

                self.declare(stream_type, pid);
                index += 5 + info;
            }
        }

        Ok(())
    }

    /// Remembers one elementary stream the program map declared.
    ///
    /// A track is created only when the stream actually carries something: a
    /// program map frequently lists a track whose packets never arrive, and an
    /// empty track is not evidence that the file holds two presentations.
    fn declare(&mut self, stream_type: u8, pid: u16) {
        match stream_type {
            0x1B if self.video_pid.is_none() => self.video_pid = Some(pid),
            0x0F if self.audio_pid.is_none() => self.audio_pid = Some(pid),
            0x24 => self.unsupported_video = Some("the transport stream carries HEVC video"),
            0x03 | 0x04 => {
                self.unsupported_audio = Some("the transport stream carries MPEG audio, not AAC")
            }
            0x81 | 0x87 => {
                self.unsupported_audio = Some("the transport stream carries Dolby audio, not AAC")
            }
            _ => {}
        }
    }

    /// Reads one packet of the video elementary stream.
    fn video_payload(&mut self, start: bool, payload: &[u8]) -> Result<(), MuxError> {
        let mut units = Vec::new();
        let mut began = 0;

        if start {
            let (header, pts, dts) = read_pes_header(payload)?;
            began = header;

            if let Some(video) = self.video.as_mut() {
                video.timed(pts, dts);
            } else {
                let mut video = VideoTrack::default();
                video.timed(pts, dts);
                self.video = Some(video);
            }
        } else if self.video.is_none() {
            self.video = Some(VideoTrack::default());
        }

        if let Some(video) = self.video.as_mut() {
            video.push(&payload[began..], &mut units)?;
        }

        for unit in units {
            self.write_video(unit)?;
        }

        Ok(())
    }

    /// Reads one packet of the audio elementary stream.
    fn audio_payload(&mut self, start: bool, payload: &[u8]) -> Result<(), MuxError> {
        let began = if start {
            read_pes_header(payload)?.0
        } else {
            0
        };

        let mut frames = Vec::new();

        if self.audio.is_none() {
            self.audio = Some(AudioTrack::default());
        }

        if let Some(audio) = self.audio.as_mut() {
            audio.push(&payload[began..], &mut frames)?;
        }

        for frame in frames {
            self.write_audio(frame)?;
        }

        Ok(())
    }

    /// Writes one complete video access unit and records its sample.
    fn write_video(&mut self, unit: CompletedUnit) -> Result<(), MuxError> {
        let Self {
            output,
            written,
            video,
            ..
        } = self;

        let track = video
            .as_mut()
            .ok_or(MuxError::Malformed("a video unit arrived before its track"))?;

        let dts = unit
            .dts
            .or(unit.pts)
            .unwrap_or_else(|| track.next_decode_time());
        let pts = unit.pts.unwrap_or(dts);
        let composition = i64::try_from(pts)
            .ok()
            .zip(i64::try_from(dts).ok())
            .and_then(|(pts, dts)| i32::try_from(pts.saturating_sub(dts)).ok())
            .unwrap_or(0);
        let size = u32::try_from(unit.data.len()).map_err(|_| MuxError::TooLarge)?;

        output.write_all(&unit.data).map_err(MuxError::Io)?;
        track.samples.push(ElementarySample {
            offset: *written,
            size,
            duration: 0,
            composition_offset: composition.max(0),
            sync: unit.sync,
        });
        track.decode_times.push(dts);
        track.last_dts = dts;
        *written = written.saturating_add(u64::from(size));

        Ok(())
    }

    /// Writes one complete AAC frame and records its sample.
    fn write_audio(&mut self, frame: Vec<u8>) -> Result<(), MuxError> {
        let Self {
            output,
            written,
            audio,
            ..
        } = self;

        let track = audio.as_mut().ok_or(MuxError::Malformed(
            "an audio frame arrived before its track",
        ))?;
        let size = u32::try_from(frame.len()).map_err(|_| MuxError::TooLarge)?;

        output.write_all(&frame).map_err(MuxError::Io)?;
        track.samples.push(ElementarySample {
            offset: *written,
            size,
            duration: AAC_FRAME_SAMPLES,
            composition_offset: 0,
            sync: true,
        });
        *written = written.saturating_add(u64::from(size));

        Ok(())
    }

    /// Closes the stream: the last access unit and the last ADTS frame are
    /// complete only because the file ended.
    fn finish(mut self) -> Result<ElementaryTrack, MuxError> {
        let mut units = Vec::new();
        let mut frames = Vec::new();

        if let Some(video) = self.video.as_mut() {
            video.flush(&mut units);
        }

        if let Some(audio) = self.audio.as_mut() {
            audio.flush(&mut frames);
        }

        for unit in units {
            self.write_video(unit)?;
        }

        for frame in frames {
            self.write_audio(frame)?;
        }

        self.output.flush().map_err(MuxError::Io)?;

        let video = self.video.take();
        let audio = self.audio.take();

        match (video, audio) {
            (Some(video), None) => video.into_track(),
            (None, Some(audio)) => audio.into_track(),
            (Some(_), Some(_)) => Err(MuxError::Unsupported(
                "the transport stream already carries both tracks",
            )),
            (None, None) => Err(self.missing_track()),
        }
    }

    /// The refusal for a stream that named no track this can read.
    fn missing_track(&self) -> MuxError {
        if let Some(reason) = self.unsupported_video {
            return MuxError::Unsupported(reason);
        }

        if let Some(reason) = self.unsupported_audio {
            return MuxError::Unsupported(reason);
        }

        MuxError::Unsupported("the transport stream declares no track this can read")
    }
}

/// A program table section being assembled across packets.
#[derive(Default)]
struct Section {
    bytes: Vec<u8>,
    expected: Option<usize>,
}

impl Section {
    /// Appends one packet's payload and returns the sections it completed.
    fn push(&mut self, payload: &[u8], start: bool) -> Result<Vec<Vec<u8>>, MuxError> {
        let mut complete = Vec::new();

        let bytes = if start {
            let pointer = usize::from(
                *payload
                    .first()
                    .ok_or(MuxError::Malformed("a program table packet is empty"))?,
            );
            let begin = 1 + pointer;

            if begin > payload.len() {
                return Err(MuxError::Malformed(
                    "a program table pointer runs past its packet",
                ));
            }

            self.bytes.clear();
            self.expected = None;
            &payload[begin..]
        } else {
            if self.expected.is_none() {
                return Ok(complete);
            }

            payload
        };

        self.bytes.extend_from_slice(bytes);

        loop {
            if self.expected.is_none() {
                if self.bytes.len() < 3 {
                    break;
                }

                // Stuffing after a section is all ones, and it is not a table.
                if self.bytes[0] == 0xFF {
                    self.bytes.clear();
                    break;
                }

                let length = (usize::from(self.bytes[1] & 0x0F) << 8) | usize::from(self.bytes[2]);
                let total = length.saturating_add(3);

                if total > MAX_SECTION {
                    return Err(MuxError::Malformed(
                        "a program table declares a length no table has",
                    ));
                }

                self.expected = Some(total);
            }

            let Some(total) = self.expected else {
                break;
            };

            if self.bytes.len() < total {
                break;
            }

            complete.push(self.bytes.drain(..total).collect());
            self.expected = None;
        }

        Ok(complete)
    }
}

/// One complete access unit, with the timing of the packet it started in.
struct CompletedUnit {
    data: Vec<u8>,
    pts: Option<u64>,
    dts: Option<u64>,
    sync: bool,
}

/// The video elementary stream, split into access units.
#[derive(Default)]
struct VideoTrack {
    /// Bytes received that do not yet make a complete NAL unit.
    pending: Vec<u8>,
    /// The access unit being assembled, as length-prefixed NAL units.
    unit: Vec<u8>,
    unit_pts: Option<u64>,
    unit_dts: Option<u64>,
    /// Timestamps of the PES packet being read.
    pts: Option<u64>,
    dts: Option<u64>,
    last_dts: u64,
    has_slice: bool,
    sync: bool,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    samples: Vec<ElementarySample>,
    decode_times: Vec<u64>,
}

impl VideoTrack {
    /// Remembers the timestamps of the PES packet that is starting.
    fn timed(&mut self, pts: Option<u64>, dts: Option<u64>) {
        self.pts = pts;
        self.dts = dts;
    }

    /// The decode time to use when a unit carries none of its own.
    fn next_decode_time(&self) -> u64 {
        match (self.decode_times.last(), self.samples.last()) {
            (Some(dts), Some(sample)) => dts.saturating_add(u64::from(sample.duration)),
            _ => self.last_dts,
        }
    }

    /// Feeds one PES payload and collects every access unit it completed.
    fn push(&mut self, payload: &[u8], complete: &mut Vec<CompletedUnit>) -> Result<(), MuxError> {
        self.pending.extend_from_slice(payload);

        while let Some(code) = find_start_code(&self.pending, 0) {
            // Anything before a start code is not part of the byte stream.
            if code > 2 {
                self.pending.drain(..code - 2);
            }

            // The next NAL unit marks the end of this one; its own start code
            // stays in the buffer for the next pass.
            let Some(next) = find_start_code(&self.pending, 3) else {
                break;
            };

            let body = &self.pending[3..next.saturating_sub(2)];
            let nal = trim_trailing_zeros(body).to_vec();
            self.pending.drain(..next.saturating_sub(2));

            self.nal(&nal, complete);
        }

        if self.pending.len() > MAX_PENDING {
            return Err(MuxError::Malformed(
                "a video NAL unit exceeds the supported size",
            ));
        }

        Ok(())
    }

    /// Emits the access unit still being assembled.
    fn flush(&mut self, complete: &mut Vec<CompletedUnit>) {
        if self.unit.is_empty() {
            return;
        }

        complete.push(CompletedUnit {
            data: std::mem::take(&mut self.unit),
            pts: self.unit_pts,
            dts: self.unit_dts.take(),
            sync: self.sync,
        });

        self.unit_pts = None;
        self.has_slice = false;
        self.sync = false;
    }

    /// Adds one NAL unit to the access unit being assembled.
    fn nal(&mut self, nal: &[u8], complete: &mut Vec<CompletedUnit>) {
        let Some(&first) = nal.first() else {
            return;
        };

        let kind = first & 0x1F;

        // An access unit delimiter always opens one; without delimiters, a slice
        // that follows another slice opens the next.
        let boundary = kind == 9 || (matches!(kind, 1..=5) && self.has_slice);

        if boundary {
            self.flush(complete);
        }

        if self.unit.is_empty() {
            self.unit_pts = self.pts;
            self.unit_dts = self.dts;
        }

        match kind {
            1..=5 => {
                self.has_slice = true;

                if kind == 5 {
                    self.sync = true;
                }
            }
            7 => self.sps = Some(nal.to_vec()),
            8 => self.pps = Some(nal.to_vec()),
            _ => {}
        }

        let Ok(size) = u32::try_from(nal.len()) else {
            return;
        };

        self.unit.extend_from_slice(&size.to_be_bytes());
        self.unit.extend_from_slice(nal);
    }

    /// Finishes the track: durations, codec configuration and total length.
    fn into_track(mut self) -> Result<ElementaryTrack, MuxError> {
        if self.samples.is_empty() {
            return Err(MuxError::Malformed(
                "no video sample survived the transport stream",
            ));
        }

        let sps = self.sps.take().ok_or(MuxError::Malformed(
            "the video has no sequence parameter set",
        ))?;
        let pps = self.pps.take().ok_or(MuxError::Malformed(
            "the video has no picture parameter set",
        ))?;
        let configuration = avc_configuration(&sps, &pps)?;
        let (width, height) = dimensions(&sps)?;

        fill_durations(&mut self.samples, &self.decode_times);

        let duration = total_duration(&self.samples);

        Ok(ElementaryTrack {
            codec: Codec::Avc {
                configuration,
                width,
                height,
            },
            timescale: VIDEO_TIMESCALE,
            samples: self.samples,
            duration,
        })
    }
}

/// The audio elementary stream, split into AAC frames.
#[derive(Default)]
struct AudioTrack {
    pending: Vec<u8>,
    sample_rate: Option<u32>,
    channel_count: u16,
    object_type: u8,
    frequency_index: u8,
    samples: Vec<ElementarySample>,
}

impl AudioTrack {
    /// Feeds one PES payload and collects every complete ADTS frame.
    fn push(&mut self, payload: &[u8], frames: &mut Vec<Vec<u8>>) -> Result<(), MuxError> {
        self.pending.extend_from_slice(payload);

        loop {
            // A packed-audio segment can open with an ID3 tag that carries its
            // timing. The tag is not audio, so it is stepped over whole rather
            // than mistaken for a frame or searched for a sync word.
            if self.pending.starts_with(b"ID3") {
                if self.pending.len() < 10 {
                    break;
                }

                let Some(tag) = id3v2_length(&self.pending) else {
                    break;
                };

                if self.pending.len() < tag {
                    break;
                }

                self.pending.drain(..tag);
                continue;
            }

            if self.pending.len() < 7 {
                break;
            }

            if self.pending[0] != 0xFF || (self.pending[1] & 0xF6) != 0xF0 {
                // Not on a frame boundary: step to the next possible sync word
                // instead of failing, because a packet may start mid-frame.
                let Some(next) = find_adts_sync(&self.pending, 1) else {
                    self.pending.clear();
                    break;
                };

                self.pending.drain(..next);
                continue;
            }

            let protection_absent = self.pending[1] & 0x01;
            let header = if protection_absent == 0 { 9 } else { 7 };

            if self.pending.len() < header {
                break;
            }

            let profile = (self.pending[2] >> 6) & 0x03;
            let frequency_index = (self.pending[2] >> 2) & 0x0F;
            let channel_configuration = ((self.pending[2] & 0x01) << 2) | (self.pending[3] >> 6);
            let length = (usize::from(self.pending[3] & 0x03) << 11)
                | (usize::from(self.pending[4]) << 3)
                | (usize::from(self.pending[5]) >> 5);

            if length < header {
                return Err(MuxError::Malformed(
                    "an AAC frame is shorter than its own header",
                ));
            }

            if self.pending.len() < length {
                break;
            }

            let rate = *SAMPLE_RATES
                .get(usize::from(frequency_index))
                .ok_or(MuxError::Malformed("an AAC frame names no sample rate"))?;

            if channel_configuration == 0 {
                return Err(MuxError::Unsupported(
                    "the AAC audio leaves its channel layout to the stream",
                ));
            }

            match self.sample_rate {
                Some(known) if known != rate => {
                    return Err(MuxError::Unsupported(
                        "the AAC audio changes sample rate partway through",
                    ));
                }
                Some(_) => {}
                None => {
                    self.sample_rate = Some(rate);
                    self.frequency_index = frequency_index;
                    self.object_type = profile + 1;
                    self.channel_count = u16::from(channel_configuration);
                }
            }

            frames.push(self.pending[header..length].to_vec());
            self.pending.drain(..length);
        }

        if self.pending.len() > MAX_PENDING {
            return Err(MuxError::Malformed(
                "an AAC frame exceeds the supported size",
            ));
        }

        Ok(())
    }

    /// Emits the frame still being assembled, if any.
    fn flush(&mut self, frames: &mut Vec<Vec<u8>>) {
        if self.pending.len() >= 7 && self.pending[0] == 0xFF && (self.pending[1] & 0xF6) == 0xF0 {
            let header = if self.pending[1] & 0x01 == 0 { 9 } else { 7 };
            let length = (usize::from(self.pending[3] & 0x03) << 11)
                | (usize::from(self.pending[4]) << 3)
                | (usize::from(self.pending[5]) >> 5);

            if length >= header && self.pending.len() >= length {
                frames.push(self.pending[header..length].to_vec());
            }
        }

        self.pending.clear();
    }

    /// Finishes the track from the samples already written.
    fn into_track(self) -> Result<ElementaryTrack, MuxError> {
        if self.samples.is_empty() {
            return Err(MuxError::Malformed(
                "no AAC frame survived the transport stream",
            ));
        }

        let sample_rate = self
            .sample_rate
            .ok_or(MuxError::Malformed("the AAC audio declares no sample rate"))?;

        // AudioSpecificConfig: the object type, the sample rate index and the
        // channel count, each at a fixed width.
        let configuration = (u16::from(self.object_type) << 11)
            | (u16::from(self.frequency_index) << 7)
            | (self.channel_count << 3);

        let duration = total_duration(&self.samples);

        Ok(ElementaryTrack {
            codec: Codec::Aac {
                configuration,
                channel_count: self.channel_count,
                sample_rate,
            },
            timescale: sample_rate,
            samples: self.samples,
            duration,
        })
    }
}

/// Reads the header of a PES packet and returns where its payload begins.
fn read_pes_header(payload: &[u8]) -> Result<(usize, Option<u64>, Option<u64>), MuxError> {
    if payload.len() < 9 || payload[..3] != [0x00, 0x00, 0x01] {
        return Err(MuxError::Malformed(
            "a packet that should start a PES packet does not",
        ));
    }

    // Streams that carry no optional header: padding, private stream 2, ECM,
    // EMM, the program stream directory, DSM-CC and H.222.1 type E.
    if matches!(
        payload[3],
        0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8 | 0xFF
    ) {
        return Ok((6, None, None));
    }

    if payload[6] & 0xC0 != 0x80 {
        return Err(MuxError::Malformed("a PES packet is not MPEG-2 shaped"));
    }

    let flags = payload[7];
    let length = usize::from(payload[8]);
    let mut offset = 9;
    let mut pts = None;
    let mut dts = None;

    if flags & 0x80 != 0 {
        let bytes = payload
            .get(offset..offset + 5)
            .ok_or(MuxError::Malformed("a PES timestamp runs past its packet"))?;
        pts = Some(read_timestamp(bytes));
        offset += 5;
    }

    if flags & 0x40 != 0 {
        let bytes = payload
            .get(offset..offset + 5)
            .ok_or(MuxError::Malformed("a PES timestamp runs past its packet"))?;
        dts = Some(read_timestamp(bytes));
    }

    let end = 9 + length;

    if end > payload.len() {
        return Err(MuxError::Malformed(
            "a PES header runs past the packet that starts it",
        ));
    }

    Ok((end, pts, dts))
}

/// Reads the 33-bit timestamp of five PES bytes.
fn read_timestamp(bytes: &[u8]) -> u64 {
    ((u64::from(bytes[0] >> 1) & 0x07) << 30)
        | (u64::from(bytes[1]) << 22)
        | ((u64::from(bytes[2] >> 1) & 0x7F) << 15)
        | (u64::from(bytes[3]) << 7)
        | ((u64::from(bytes[4]) >> 1) & 0x7F)
}

/// Where the first Annex B start code begins at or after `from`, as the index of
/// its `0x01` byte.
fn find_start_code(bytes: &[u8], from: usize) -> Option<usize> {
    let mut index = from;

    while index + 2 < bytes.len() {
        if bytes[index] == 0 && bytes[index + 1] == 0 && bytes[index + 2] == 0x01 {
            return Some(index + 2);
        }

        index += 1;
    }

    None
}

/// Where the first ADTS sync word begins at or after `from`.
fn find_adts_sync(bytes: &[u8], from: usize) -> Option<usize> {
    (from..bytes.len().saturating_sub(1))
        .find(|&index| bytes[index] == 0xFF && (bytes[index + 1] & 0xF6) == 0xF0)
}

/// Drops the zero bytes a start code leaves in front of a NAL unit.
fn trim_trailing_zeros(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();

    while end > 0 && bytes[end - 1] == 0 {
        end -= 1;
    }

    &bytes[..end]
}

/// Fills in each video sample's duration from the decode time of the next one.
fn fill_durations(samples: &mut [ElementarySample], decode_times: &[u64]) {
    let mut previous = DEFAULT_FRAME_DURATION;

    for index in 0..samples.len() {
        let duration = match decode_times.get(index + 1) {
            Some(&next) => {
                let delta = next.saturating_sub(decode_times[index]);

                if delta > 0 && delta <= u64::from(MAX_FRAME_DURATION) {
                    u32::try_from(delta).unwrap_or(previous)
                } else {
                    previous
                }
            }
            None => previous,
        };

        samples[index].duration = duration;
        previous = duration;
    }
}

/// The length of a track, as the sum of its sample durations.
fn total_duration(samples: &[ElementarySample]) -> u64 {
    samples
        .iter()
        .map(|sample| u64::from(sample.duration))
        .fold(0_u64, u64::saturating_add)
}

/// Assembles the `avcC` body from the sequence and picture parameter sets.
fn avc_configuration(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>, MuxError> {
    if sps.len() < 4 {
        return Err(MuxError::Malformed(
            "the sequence parameter set is too short to describe a profile",
        ));
    }

    let sps_length = u16::try_from(sps.len())
        .map_err(|_| MuxError::Malformed("the sequence parameter set is too long"))?;
    let pps_length = u16::try_from(pps.len())
        .map_err(|_| MuxError::Malformed("the picture parameter set is too long"))?;

    let mut configuration = Vec::with_capacity(sps.len() + pps.len() + 11);

    configuration.push(1); // configurationVersion
    configuration.extend_from_slice(&sps[1..4]); // profile, compatibility, level
    configuration.push(0xFF); // lengthSizeMinusOne: four-byte lengths
    configuration.push(0xE1); // one sequence parameter set
    configuration.extend_from_slice(&sps_length.to_be_bytes());
    configuration.extend_from_slice(sps);
    configuration.push(1); // one picture parameter set
    configuration.extend_from_slice(&pps_length.to_be_bytes());
    configuration.extend_from_slice(pps);

    Ok(configuration)
}

/// Reads the picture size a sequence parameter set declares.
fn dimensions(sps: &[u8]) -> Result<(u16, u16), MuxError> {
    let rbsp = remove_emulation_prevention(sps);
    let mut bits = Bits::new(&rbsp);

    bits.read(8)?; // NAL unit header
    let profile = bits.read(8)?;
    bits.read(8)?; // constraint flags
    bits.read(8)?; // level
    bits.unsigned()?; // sequence parameter set id

    let mut chroma = 1_u64;

    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma = bits.unsigned()?;

        if chroma == 3 {
            bits.read(1)?; // separate_colour_plane_flag
        }

        bits.unsigned()?; // bit_depth_luma_minus8
        bits.unsigned()?; // bit_depth_chroma_minus8
        bits.read(1)?; // qpprime_y_zero_transform_bypass_flag

        if bits.read(1)? == 1 {
            let lists = if chroma != 3 { 8 } else { 12 };

            for index in 0..lists {
                if bits.read(1)? == 1 {
                    skip_scaling_list(&mut bits, if index < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    bits.unsigned()?; // log2_max_frame_num_minus4

    match bits.unsigned()? {
        0 => {
            bits.unsigned()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            bits.read(1)?; // delta_pic_order_always_zero_flag
            bits.signed()?;
            bits.signed()?;

            let cycle = bits.unsigned()?;

            for _ in 0..cycle {
                bits.signed()?;
            }
        }
        _ => {}
    }

    bits.unsigned()?; // max_num_ref_frames
    bits.read(1)?; // gaps_in_frame_num_value_allowed_flag

    let width_units = bits.unsigned()?.saturating_add(1);
    let height_units = bits.unsigned()?.saturating_add(1);
    let frame_mbs_only = bits.read(1)?;

    if frame_mbs_only == 0 {
        bits.read(1)?; // mb_adaptive_frame_field_flag
    }

    bits.read(1)?; // direct_8x8_inference_flag

    let mut width = width_units.saturating_mul(16);
    let mut height = height_units.saturating_mul(if frame_mbs_only == 1 { 16 } else { 32 });

    if bits.read(1)? == 1 {
        let left = bits.unsigned()?;
        let right = bits.unsigned()?;
        let top = bits.unsigned()?;
        let bottom = bits.unsigned()?;

        // Cropping is expressed in chroma units; a 4:2:0 picture has two luma
        // samples per chroma sample in both directions.
        let (unit_x, unit_y) = match chroma {
            1 => (2_u64, 2_u64),
            2 => (2, 1),
            _ => (1, 1),
        };

        width = width.saturating_sub(left.saturating_add(right).saturating_mul(unit_x));
        height = height.saturating_sub(
            top.saturating_add(bottom)
                .saturating_mul(unit_y)
                .saturating_mul(if frame_mbs_only == 1 { 1 } else { 2 }),
        );
    }

    let width = u16::try_from(width)
        .map_err(|_| MuxError::Malformed("the sequence parameter set declares no usable width"))?;
    let height = u16::try_from(height)
        .map_err(|_| MuxError::Malformed("the sequence parameter set declares no usable height"))?;

    if width == 0 || height == 0 {
        return Err(MuxError::Malformed(
            "the sequence parameter set declares an empty picture",
        ));
    }

    Ok((width, height))
}

/// Removes the `0x03` bytes that keep a NAL unit from looking like a start code.
fn remove_emulation_prevention(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut zeros = 0;

    for &byte in bytes {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }

        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }

    out
}

/// Skips a scaling list, which is delta-coded and has no fixed length.
fn skip_scaling_list(bits: &mut Bits<'_>, size: usize) -> Result<(), MuxError> {
    let mut last = 8_i64;
    let mut next = 8_i64;

    for _ in 0..size {
        if next != 0 {
            let delta = bits.signed()?;
            next = (last + delta + 256) % 256;
        }

        if next != 0 {
            last = next;
        }
    }

    Ok(())
}

/// Reads the bits a sequence parameter set is made of.
struct Bits<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    /// Reads one bit.
    fn read(&mut self, count: usize) -> Result<u64, MuxError> {
        let mut value = 0_u64;

        for _ in 0..count {
            let byte = *self
                .bytes
                .get(self.position / 8)
                .ok_or(MuxError::Malformed("a parameter set ends too early"))?;
            let bit = (byte >> (7 - (self.position % 8))) & 1;

            value = (value << 1) | u64::from(bit);
            self.position += 1;
        }

        Ok(value)
    }

    /// Reads an unsigned Exp-Golomb code.
    fn unsigned(&mut self) -> Result<u64, MuxError> {
        let mut zeros = 0;

        while self.read(1)? == 0 {
            zeros += 1;

            if zeros > 32 {
                return Err(MuxError::Malformed(
                    "a parameter set holds an impossible Exp-Golomb code",
                ));
            }
        }

        if zeros == 0 {
            return Ok(0);
        }

        let rest = self.read(zeros)?;

        Ok((1_u64 << zeros) - 1 + rest)
    }

    /// Reads a signed Exp-Golomb code.
    fn signed(&mut self) -> Result<i64, MuxError> {
        let value = self.unsigned()?;

        Ok(if value % 2 == 0 {
            -i64::try_from(value / 2).unwrap_or(i64::MAX)
        } else {
            i64::try_from(value.div_ceil(2)).unwrap_or(i64::MAX)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one transport-stream packet with the given payload.
    fn packet(pid: u16, unit_start: bool, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x47];
        packet.push(((pid >> 8) as u8 & 0x1F) | if unit_start { 0x40 } else { 0 });
        packet.push(pid as u8);
        packet.push(0x10); // payload only, continuity counter zero

        let mut body = Vec::new();
        body.extend_from_slice(payload);
        body.resize(PACKET - 4, 0xFF);
        packet.extend_from_slice(&body);

        packet
    }

    /// Wraps a section in a packet payload with a zero pointer field.
    fn section_payload(section: &[u8]) -> Vec<u8> {
        let mut payload = vec![0x00];
        payload.extend_from_slice(section);
        payload
    }

    /// A PAT naming one program and its PMT.
    fn pat(pmt_pid: u16) -> Vec<u8> {
        let mut body = vec![0x00, 0x01, 0xC1, 0x00, 0x00]; // transport id, version, sections
        body.extend_from_slice(&1_u16.to_be_bytes()); // program number
        body.push(0xE0 | (pmt_pid >> 8) as u8);
        body.push(pmt_pid as u8);

        section(0x00, &body)
    }

    /// A PMT naming one video and one audio stream.
    fn pmt(video_pid: u16, audio_pid: u16) -> Vec<u8> {
        let mut body = vec![0x00, 0x01, 0xC1, 0x00, 0x00];
        body.extend_from_slice(&0x1FFF_u16.to_be_bytes()); // no PCR
        body.extend_from_slice(&0xF000_u16.to_be_bytes()); // no program descriptors
        body.extend_from_slice(&[0x1B]);
        body.push(0xE0 | (video_pid >> 8) as u8);
        body.push(video_pid as u8);
        body.extend_from_slice(&0xF000_u16.to_be_bytes());
        body.push(0x0F);
        body.push(0xE0 | (audio_pid >> 8) as u8);
        body.push(audio_pid as u8);
        body.extend_from_slice(&0xF000_u16.to_be_bytes());

        section(0x02, &body)
    }

    /// Wraps a table body in a section header and a placeholder CRC.
    fn section(table: u8, body: &[u8]) -> Vec<u8> {
        let length = body.len() + 4;
        let mut section = vec![table, 0xB0 | ((length >> 8) as u8 & 0x0F), length as u8];
        section.extend_from_slice(body);
        section.extend_from_slice(&[0; 4]);

        section
    }

    /// A PES packet carrying `payload`, with a PTS and optionally a DTS.
    fn pes(payload: &[u8], pts: u64, dts: Option<u64>) -> Vec<u8> {
        let mut header = vec![0x00, 0x00, 0x01, 0xE0];
        header.extend_from_slice(&0_u16.to_be_bytes()); // length, unbounded
        header.push(0x80); // MPEG-2 header
        header.push(if dts.is_some() { 0xC0 } else { 0x80 });
        header.push(if dts.is_some() { 10 } else { 5 });
        header.extend_from_slice(&timestamp(pts, 0b0010));

        if let Some(dts) = dts {
            header.extend_from_slice(&timestamp(dts, 0b0001));
        }

        header.extend_from_slice(payload);

        header
    }

    /// Encodes a 33-bit timestamp in the PES marker format.
    fn timestamp(value: u64, prefix: u8) -> [u8; 5] {
        [
            (prefix << 4) | (((value >> 30) as u8 & 0x07) << 1) | 0x01,
            (value >> 22) as u8,
            (((value >> 15) as u8 & 0x7F) << 1) | 0x01,
            (value >> 7) as u8,
            ((value as u8 & 0x7F) << 1) | 0x01,
        ]
    }

    /// A NAL unit with the given header byte and body.
    fn nal(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut unit = vec![0x00, 0x00, 0x00, 0x01, kind];
        unit.extend_from_slice(body);

        unit
    }

    /// A sequence parameter set for a 1280x720 baseline picture.
    fn sps() -> Vec<u8> {
        let mut bits = BitWriter::default();
        bits.unsigned(0); // sps id
        bits.unsigned(0); // log2_max_frame_num_minus4
        bits.unsigned(0); // pic_order_cnt_type
        bits.unsigned(0); // log2_max_pic_order_cnt_lsb_minus4
        bits.unsigned(1); // max_num_ref_frames
        bits.bit(0); // gaps_in_frame_num_value_allowed_flag
        bits.unsigned(79); // width in 16-pixel units, minus one
        bits.unsigned(44); // height in 16-pixel units, minus one
        bits.bit(1); // frame_mbs_only_flag
        bits.bit(0); // direct_8x8_inference_flag
        bits.bit(0); // frame cropping
        bits.bit(0); // vui parameters

        let mut body = vec![0x42, 0xC0, 0x1E]; // baseline, level 3.0
        body.extend_from_slice(&bits.finish());
        body.insert(0, 0x67);

        body
    }

    /// Writes the bits an SPS is made of, for the fixtures only.
    #[derive(Default)]
    struct BitWriter {
        bits: Vec<u8>,
    }

    impl BitWriter {
        fn bit(&mut self, value: u8) {
            self.bits.push(value & 1);
        }

        fn unsigned(&mut self, value: u64) {
            let value = value + 1;
            let width = 64 - value.leading_zeros();

            for _ in 0..width.saturating_sub(1) {
                self.bit(0);
            }

            for index in (0..width).rev() {
                self.bit(((value >> index) as u8) & 1);
            }
        }

        fn finish(mut self) -> Vec<u8> {
            self.bit(1);

            while !self.bits.len().is_multiple_of(8) {
                self.bit(0);
            }

            self.bits
                .chunks(8)
                .map(|chunk| chunk.iter().fold(0_u8, |byte, &bit| (byte << 1) | bit))
                .collect()
        }
    }

    /// An ADTS header carrying one stereo AAC-LC frame at 44.1 kHz.
    fn adts(frame_length: usize) -> Vec<u8> {
        let length = frame_length + 7;

        vec![
            0xFF,
            0xF1, // MPEG-4, layer 0, no CRC
            0x50, // AAC-LC, 44.1 kHz
            0x80 | ((length >> 11) as u8 & 0x03),
            (length >> 3) as u8,
            ((length as u8 & 0x07) << 5) | 0x1F,
            0xFC,
        ]
    }

    /// One ADTS frame whose body is a repeat of one byte.
    fn adts_frame(byte: u8, size: usize) -> Vec<u8> {
        let mut frame = adts(size);
        frame.extend(std::iter::repeat_n(byte, size));

        frame
    }

    /// An ID3v2 tag holding `size` bytes, as a packed-audio segment carries.
    fn id3(size: usize) -> Vec<u8> {
        let mut tag = vec![b'I', b'D', b'3', 4, 0, 0];
        tag.push(((size >> 21) as u8) & 0x7F);
        tag.push(((size >> 14) as u8) & 0x7F);
        tag.push(((size >> 7) as u8) & 0x7F);
        tag.push((size as u8) & 0x7F);
        tag.extend(std::iter::repeat_n(0x00, size));

        tag
    }

    #[test]
    fn reads_video_access_units_and_their_timing() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let source = directory.path().join("video.ts");
        let output = directory.path().join("video.es");

        let mut stream = Vec::new();
        stream.extend(packet(0, true, &section_payload(&pat(0x1000))));
        stream.extend(packet(0x1000, true, &section_payload(&pmt(0x0100, 0x0101))));

        let first = [
            nal(0x09, &[0xF0]),
            nal(0x67, &sps()[1..]),
            nal(0x68, &[0xCE, 0x3C, 0x80]),
            nal(0x65, &[0x88, 0x84, 0x00, 0x33]),
        ]
        .concat();
        let second = [nal(0x09, &[0xF0]), nal(0x41, &[0x9A, 0x02, 0x03])].concat();

        stream.extend(packet(0x0100, true, &pes(&first, 90_000, None)));
        stream.extend(packet(0x0100, true, &pes(&second, 93_000, None)));

        std::fs::write(&source, &stream).expect("the fixture must be written");

        let track = demux(&source, &output).expect("the stream must be read");

        assert_eq!(track.timescale, VIDEO_TIMESCALE);
        assert_eq!(track.samples.len(), 2);
        assert_eq!(track.samples[0].duration, 3_000);
        assert_eq!(track.samples[1].duration, 3_000);
        assert!(track.samples[0].sync);
        assert!(!track.samples[1].sync);
        assert_eq!(track.duration, 6_000);

        let Codec::Avc {
            configuration,
            width,
            height,
        } = &track.codec
        else {
            panic!("the track must be H.264");
        };

        assert_eq!((*width, *height), (1280, 720));
        assert_eq!(configuration[0], 1);
        assert_eq!(configuration[1], 0x42);
        assert_eq!(configuration[4], 0xFF);
        assert_eq!(
            u16::from_be_bytes([configuration[6], configuration[7]]) as usize,
            sps().len()
        );

        let bytes = std::fs::read(&output).expect("the elementary file must be readable");
        assert_eq!(
            bytes.len() as u64,
            u64::from(track.samples[0].size + track.samples[1].size)
        );
    }

    #[test]
    fn reads_adts_and_strips_its_header() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let source = directory.path().join("audio.ts");
        let output = directory.path().join("audio.es");

        let mut stream = Vec::new();
        stream.extend(packet(0, true, &section_payload(&pat(0x1000))));
        stream.extend(packet(0x1000, true, &section_payload(&pmt(0x0100, 0x0101))));

        let frames = [
            adts_frame(0x11, 4),
            adts_frame(0x22, 4),
            adts_frame(0x33, 4),
        ]
        .concat();

        stream.extend(packet(0x0101, true, &pes(&frames, 0, None)));
        std::fs::write(&source, &stream).expect("the fixture must be written");

        let track = demux(&source, &output).expect("the stream must be read");

        assert_eq!(track.timescale, 44_100);
        assert_eq!(track.samples.len(), 3);
        assert_eq!(track.samples[0].duration, AAC_FRAME_SAMPLES);
        assert_eq!(track.duration, 3 * u64::from(AAC_FRAME_SAMPLES));
        assert_eq!(track.samples[0].size, 4);

        let Codec::Aac {
            configuration,
            channel_count,
            sample_rate,
        } = track.codec
        else {
            panic!("the track must be AAC");
        };

        assert_eq!((configuration >> 11) & 0x1F, 2); // AAC-LC
        assert_eq!((configuration >> 7) & 0x0F, 4); // 44.1 kHz
        assert_eq!((configuration >> 3) & 0x0F, 2); // stereo
        assert_eq!(channel_count, 2);
        assert_eq!(sample_rate, 44_100);

        let bytes = std::fs::read(&output).expect("the elementary file must be readable");
        assert_eq!(
            bytes,
            [
                0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x22, 0x22, 0x33, 0x33, 0x33, 0x33
            ]
        );
    }

    #[test]
    fn oversized_video_pending_is_refused() {
        let mut video = VideoTrack::default();
        let mut complete = Vec::new();
        let error = video
            .push(&vec![0; MAX_PENDING + 1], &mut complete)
            .expect_err("an oversized NAL must not be dropped silently");

        assert!(matches!(error, MuxError::Malformed(_)));
    }

    #[test]
    fn steps_over_the_id3_tags_that_open_packed_audio_segments() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let source = directory.path().join("audio.ts");
        let output = directory.path().join("audio.es");

        let mut stream = Vec::new();
        stream.extend(packet(0, true, &section_payload(&pat(0x1000))));
        stream.extend(packet(0x1000, true, &section_payload(&pmt(0x0100, 0x0101))));

        // A packed-audio segment carries a tag before its frames; a download
        // that concatenates segments therefore holds one between them too.
        let frames = [id3(16), adts_frame(0x11, 4), id3(16), adts_frame(0x22, 4)].concat();

        stream.extend(packet(0x0101, true, &pes(&frames, 0, None)));
        std::fs::write(&source, &stream).expect("the fixture must be written");

        let track = demux(&source, &output).expect("the stream must be read");

        assert_eq!(track.samples.len(), 2);
        assert_eq!(track.timescale, 44_100);

        let bytes = std::fs::read(&output).expect("the elementary file must be readable");
        assert_eq!(bytes, [0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x22, 0x22]);
    }

    #[test]
    fn refuses_a_stream_whose_video_is_not_something_it_reads() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let source = directory.path().join("hevc.ts");
        let output = directory.path().join("hevc.es");

        let mut stream = Vec::new();
        stream.extend(packet(0, true, &section_payload(&pat(0x1000))));

        let mut body = vec![0x00, 0x01, 0xC1, 0x00, 0x00];
        body.extend_from_slice(&0x1FFF_u16.to_be_bytes());
        body.extend_from_slice(&0xF000_u16.to_be_bytes());
        body.push(0x24); // HEVC
        body.push(0xE0);
        body.push(0x00);
        body.extend_from_slice(&0xF000_u16.to_be_bytes());

        stream.extend(packet(
            0x1000,
            true,
            &section_payload(&section(0x02, &body)),
        ));
        std::fs::write(&source, &stream).expect("the fixture must be written");

        let error = demux(&source, &output).expect_err("HEVC must be refused");

        assert!(
            matches!(error, MuxError::Unsupported(_)),
            "expected a refusal, got {error}"
        );
    }

    #[test]
    fn reads_a_file_that_is_already_adts() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let source = directory.path().join("audio.aac");
        let output = directory.path().join("audio.es");

        let frames = [adts_frame(0x44, 6), adts_frame(0x55, 6)].concat();
        std::fs::write(&source, &frames).expect("the fixture must be written");

        let track = demux_adts(&source, &output).expect("the file must be read");

        assert_eq!(track.samples.len(), 2);
        assert_eq!(track.timescale, 44_100);

        let bytes = std::fs::read(&output).expect("the elementary file must be readable");
        assert_eq!(
            bytes,
            [0x44; 6]
                .iter()
                .chain(&[0x55; 6])
                .copied()
                .collect::<Vec<_>>()
        );
    }
}
