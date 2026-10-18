# Media capture and segmented streaming

This document records the problem browser media capture solves, the design
decisions behind Gunda's capture pipeline, and what the `gunda-stream` engine
does today. It is the design record for `crates/gunda-stream`.

## 1. What browser media capture has to solve

Three properties of the web shape the design:

- **The page never exposes a file.** Media arrives as a manifest (HLS `.m3u8`,
  DASH `.mpd`, Smooth Streaming) followed by segments, or as one progressive
  response. Reading the page's markup cannot produce a download: media URLs are
  built at runtime and are frequently signed per session.
- **Requests are the reliable signal.** The response that carries the media is
  the thing worth copying, so a capture pipeline observes the requests a page
  makes and classifies responses by the MIME type the server returns, not by
  the shape of the URL. Sites can hide media behind opaque query strings and
  change paths at will, but a player only accepts a manifest with the right
  content type.
- **A presentation is not a file.** At most quality levels the picture and the
  sound travel as two independent tracks with separate URLs. Concatenating those
  downloads cannot produce a playable result: a container has to be written that
  describes both at once. That is why "download this video" ends in a muxing
  step rather than a copy.

### The shape of a capture pipeline

```
[ page world ]        [ isolated world ]      [ service worker ]      [ native ]
request instrumentation <-> DOM observation <-> response sniffing  -->  local service
patch XHR/fetch/MSE        geometry + events    MIME classification
                                        |
                          ws://127.0.0.1:PORT   or   native messaging
```

The roles are fixed even though implementations differ:

- **Page-world instrumentation.** A script injected into the page's own
  JavaScript context patches `XMLHttpRequest`, `fetch` and
  `MediaSource`/`SourceBuffer`. Appended source buffers are decoded as text and
  scanned for URLs, which catches players that build segment URLs at runtime.
- **Request correlation.** A detected player element is tied to the response
  that carries its media by marking the requests the extension can see and
  matching the element against the marked response, rather than guessing from
  URL shape.
- **Response sniffing.** `webRequest` listeners cover document, frame, object,
  image, XHR, media and other request types, and a MIME-to-extension table maps
  the manifest and segment types a site may serve (`M3U8`, `MPD`, `F4M`, `M4S`,
  `TS`, `UMP`, `DFXP`/`TTML`, `WEBM`, `MP4`).
- **A local transport.** An extension cannot open a raw socket, so candidates
  reach the desktop application either over a loopback WebSocket or through the
  browser's native-messaging bridge, which needs an installed host manifest and
  a helper executable.
- **A native overlay, if the UI is one.** A window drawn over a video cannot
  receive DOM input, so the content script has to report the element's
  rectangle, device-pixel ratio, visibility and clipped ancestor box, the
  window's inner size, and mirror mouse, keyboard, scroll, resize and blur
  events.

### Consequences for the engine

Two cases have to be told apart. A presentation that already carries muxed media
is downloaded as it stands. A presentation that splits audio from video cannot
yield a single-file container without a muxer: concatenating its parts can never
do it. Gunda therefore muxes in-process; there is no external tool to install
and no subprocess to launch.

## 2. Design decisions

1. **The browser side pushes to a local listener.** Gunda listens on loopback
   and the extension delivers detected media candidates to it. The desktop
   application decides what a candidate becomes.
2. **Detection keys off MIME type, not URL shape.** The manifest types Gunda
   recognises (`application/vnd.apple.mpegurl`, `application/dash+xml`,
   `application/vnd.yt-ump`, `application/octet-stream-m3u8`, …) are what make
   detection work across sites without per-site code.
3. **Correlation by request tagging.** The extension marks requests it can see
   so a detected element can be tied to the response that carries it.
4. **Segmented download is part of the engine, not a parallel lifecycle.** It
   plugs into the existing `DownloadExecutor` seam
   (`crates/gunda-core/src/application/execution.rs`). `ResourceKind::Hls` /
   `ResourceKind::Dash` and `DownloadOrigin::Browser { page_url, page_title }`
   already exist in the domain for exactly this.

Choices made against alternatives:

- **No per-site player adapters.** Site-specific selectors are a maintenance
  treadmill and break silently whenever a player changes. Gunda relies on
  manifest MIME detection and the reported `<video>`/MSE sources instead.
- **JSON over WebSocket, not a bespoke binary framing.** There is no legacy
  client to stay compatible with, and a self-describing protocol is easier to
  audit.
- **An authenticated loopback port.** Gunda requires a shared secret,
  established at extension-pairing time, on every connection and every message,
  so a page cannot inject downloads into the application.

## 3. The segmented download engine (`crates/gunda-stream`)

Implemented today:

- `hls`: playlist parsing (master and media), attribute lists with quoted
  values, `EXT-X-STREAM-INF`, `EXT-X-MEDIA`, `EXT-X-I-FRAME-STREAM-INF`,
  `EXTINF`, `EXT-X-BYTERANGE` (including offset continuation),
  `EXT-X-MAP`, `EXT-X-KEY`, `EXT-X-DISCONTINUITY`, `EXT-X-MEDIA-SEQUENCE`,
  `EXT-X-ENDLIST`. Unknown tags are ignored as the specification requires.
- `dash`: a bounded XML reader (`xml`, no new dependency) plus MPD parsing for
  `Period`/`AdaptationSet`/`Representation` with `SegmentTemplate` (including
  `SegmentTimeline` and `$Number$`/`$Time$`/`$RepresentationID$`/`$Bandwidth$`
  substitution with `%0Nd` padding), inline `SegmentList` with `mediaRange` byte
  ranges, and single-file representations. `SegmentTemplate` attributes inherit
  from representation to adaptation set to period.
- `plan`: resolves a parsed manifest into an ordered list of absolute
  `http`/`https` resources with optional byte ranges — initialization segments
  interleaved exactly where they apply — and refuses a plan it cannot execute
  honestly. A presentation that keeps its sound in a track of its own is planned
  as two tracks: every resource carries the track it belongs to
  (`StreamResource::track`), and `with_audio` merges the two plans into one.
  MPEG-DASH video and audio adaptation sets are planned together this way, and an
  HLS master playlist's variant is paired with the audio rendition it names
  (`EXT-X-MEDIA:TYPE=AUDIO` with a `URI`, the `DEFAULT=YES` one first).
- `executor`: `StreamExecutor` implements `DownloadExecutor`. It fetches the
  manifest, follows one master-playlist indirection, plans the presentation,
  then appends every planned resource in order to the ordinary
  `.gunda-<id>.part` staging file. A resource that is one byte interval of a
  larger file is fetched with `bytes=first-last` and only that interval is
  appended, so single-file HLS playlists and DASH `SegmentList` work. A plan of
  two tracks is fetched into `.part.video` and `.part.audio` and then muxed into
  the partial file (section 3.3); those staging files are removed whatever
  happens, through `StagingTracks`. Progress is reported as bytes are appended,
  the staging file is created exclusively (never replacing an existing path),
  cancellation is checked between resources, and publication goes through
  `finalize_download` so filename validation and conflict policy are shared with
  plain HTTP downloads rather than reimplemented.
- `mux`: a fragmented-MP4 demuxer and a plain-MP4 writer, with no codec
  knowledge and no external tool. See section 3.3.
- `container`: the media's container is read from the media itself. The first
  planned resource is read up to `SNIFF_BYTES` (2048) and recognised from its
  leading bytes — ISO base media by brand and by its `hdlr` track handler
  (fragmented MP4, or a `M4A `/`M4B `/`M4P `/`F4A ` brand, or an all-`soun`
  handler scan), Matroska/WebM by EBML, MPEG-TS by packet sync bytes, ADTS by
  frame sync, MP3 by frame sync. A byte-range resource is read at the offset the
  manifest gave it, not from zero. What the format itself implies refines this
  and never overrides it: DASH segments, and HLS segments that need an
  `EXT-X-MAP`, are ISO base media by construction; an HLS playlist without one
  carries transport-stream packets. Packed audio can carry an ID3v2 tag in front
  of every segment; the tag is stepped over, because it says when the segment
  plays rather than what codec it holds. Nothing is guessed: an unrecognised
  stream whose format implies nothing is named `.bin`.
- `presentation`: `inspect` describes what a presentation offers, without
  downloading any media — for HLS, one track per `EXT-X-STREAM-INF` variant and
  per `TYPE=AUDIO` rendition that has a playlist of its own, each with its own
  URL, resolution, bandwidth, codecs, duration and size. Describing a variant
  costs one request for its playlist, so the number described is capped
  (`MAX_DESCRIBED_VARIANTS`). A variant that names an audio rendition carries no
  sound itself, and only the presentation knows which rendition belongs with
  which quality: such a presentation is therefore described as **one track** —
  its best quality with the rendition the presentation pairs it with, sized as
  the sum of both — because offering the qualities one by one would offer silent
  downloads. Its separately addressable audio renditions are still tracks of
  their own.

Two decisions the naming and sizing rest on:

- **The output is named after the media, not the manifest.** A manifest URL
  names a playlist, so publishing its last path segment unchanged saved
  `index.m3u8` for a file that is really an MP4 or a transport stream. The
  validated stem is kept and its extension replaced with the container the bytes
  are (`output_filename`, shared with the desktop so a chosen quality keeps the
  name of the presentation rather than of the variant's own playlist, which is
  frequently also called `index.m3u8`). A filename the caller chose still wins,
  and `plan_destination` remains the only validator.
- **Sizes are exact or absent, never quietly approximate.** A declared length is
  used as it stands; otherwise every planned resource is probed with one bounded
  metadata request (`HEAD`, at most `PROBE_CONCURRENCY` = 8 in flight, at most
  `MAX_PROBED_RESOURCES` = 512 resources) and the total is the sum, so the
  progress total is measured rather than inferred. Beyond that bound the total
  is reported as unknown rather than estimated. The picker is the one place an
  estimate appears, taken from the declared bitrate and duration, always marked
  as an estimate (`~`).

Refusals are explicit and named (`UnsupportedFeature`) rather than silently
approximated:

| Feature | Why it is refused |
| --- | --- |
| `EncryptedSegments` (`EXT-X-KEY` ≠ `NONE`) | Decryption is not implemented; publishing ciphertext as a finished video would be wrong. |
| `IncompletePresentation` | Live playlists and `type="dynamic"` MPDs would produce a silently truncated file. Continuous capture is a separate mode. |
| `SeparateTracks` | Split tracks whose bytes are not a container this can mux — Matroska/WebM input, for instance — cannot be combined, and publishing only the picture would silently lose the sound. ISO base media is carried over; MPEG-TS and ADTS audio are demuxed first. |
| `MultiPeriod` | Stitching periods requires re-encode decisions. |
| `SegmentIndex` | `SegmentBase`/`sidx` enumeration is not implemented. |
| `DataUriSegments` | Inline segments are not fetchable resources. |

Because these refusals are typed, the UI can explain the reason per download
instead of showing a generic failure.

Known execution-time limits, which are typed failures rather than silent
approximation:

- **No resume.** `supports_pause()` is `false`: the engine has no record of
  which planned resources are already staged, so pausing and resuming would
  re-download or corrupt output.

## 3.1 Seams added to `gunda-http`

A protocol engine needs four things from the HTTP crate that did not exist:

- `PartialDownload::adopt` — adopt the staging file the engine wrote, so
  publication still runs through `finalize_download` (validated filename,
  conflict policy, partial-file identity).
- `plan_destination` — now public: destination naming is shared policy, and an
  engine that planned a different final path than the manager would report the
  wrong result to the user.
- `HttpInspection::staged` — internal constructor for a staging file with no
  remote validator.
- `HttpClient::open_byte_range` — fetch exactly `bytes=first-last` with no
  validator. The existing `open_range` cannot serve this: it resumes from an
  offset to the end of a representation and requires a strong entity tag. A
  server that ignores `Range` and answers `200 OK` with the whole resource is
  rejected, because appending a whole file where one segment was asked for
  would corrupt the output.

Each is additive; no existing behaviour changed.

## 3.2 The capture channel (`crates/gunda-capture`, `apps/extension`)

A browser extension cannot open a TCP socket, so something has to be on both
ends. Two transports can carry this: a loopback WebSocket, or the browser's
native-messaging bridge. Gunda implements the first one only, because it needs
no native host registration and no extra executable:

```
page ── runtime messages ──▶ extension service worker ── WebSocket ──▶ CaptureServer
        (content.js)            (background.js)          ws://127.0.0.1:PORT/capture
```

### Transport

The RFC 6455 subset needed to accept one browser connection is implemented in
`crates/gunda-capture/src/websocket.rs`: the opening handshake, text messages
(fragmented or not), ping/pong, and close. Binary messages, reserved opcodes, and
negotiated extensions are refused rather than ignored. The reasons for writing it
instead of adding a dependency:

- no WebSocket crate is in `Cargo.lock`, so a dependency would have to be
  downloaded, and the workspace keeps its dependency set small on purpose;
- the subset a single-client, text-only channel needs is bounded and testable;
- the handshake is verified against the worked example in RFC 6455 section 1.3,
  and the server is driven end to end by Node's independent WebSocket
  implementation (see the evidence below).

A frame that cannot be understood ends the connection; nothing is dropped
silently. Client frames must be masked, control frames must be short and final,
payloads are capped, and the reserved bits must be clear.

### Protocol

JSON objects tagged with a `kind` (`serde` internally tagged enums), versioned by
the greeting, limited to `MAX_MESSAGE_BYTES` (256 KiB).

| Extension → desktop | Payload |
| --- | --- |
| `hello` | `version`, `token`, `agent` — the first message on every connection |
| `candidate` | `url`, `content_type?`, `page_url`, `page_title?` |

| Desktop → extension | Meaning |
| --- | --- |
| `ready` | the greeting was accepted; `version` is the desktop's protocol version |
| `accepted` | the report is now a candidate, with its `id` |
| `duplicate` | the report matched a candidate already known, with its `id` |
| `refused` | a stable `reason` (below) |
| `malformed` | the message could not be read at all |

Compatibility rule: unknown *fields* are ignored so a newer extension keeps
working with an older Gunda; an unknown *message kind* is refused rather than
guessed at.

Refusal reasons, as sent on the wire:

| Reason | Meaning |
| --- | --- |
| `unsupported_url` | not an absolute `http`/`https` URL, or a page URL that cannot be a base |
| `unsupported_media` | not media this build recognises |
| `segment_stream` | one segment of a segmented stream (`video/mp2t`, `.ts`, `.m4s`), not a stream |
| `unsupported_version` | the extension speaks a newer protocol |

### The candidate model

A report is validated *before* it can become a candidate, so an unexecutable
resource is refused at the boundary instead of failing later in the engine:

- the media URL must be absolute `http`/`https`;
- the page URL must be an absolute URL (it becomes `DownloadOrigin::Browser`);
- the resource is classified by content type first, then by path extension
  (`hls`, `dash`, `file`, else refused);
- `video/mp2t` and `.ts`/`.m4s` paths are refused as segments: the manifest is
  the resource worth downloading, and reporting every segment would bury it;
- page titles are stripped of control characters and bounded.

The catalog holds the newest 256 candidates, newest first, and evicts the oldest
when full rather than refusing the newest: a capture list is a list of recent
discoveries, and the entry a user is most likely to want is the one that just
appeared. Deduplication is by media URL; a repeat updates the content type,
the last-seen time, and the report count instead of creating a second entry.

### Pairing and origin policy

Loopback is reachable from every page in the browser, so the port is not a
boundary. Two things are:

1. a 32-byte token (64 hex characters) generated from the operating system's
   randomness on first run, stored in the application data directory, pasted by
   the user into the extension, compared without an early exit, and never
   printable through `Debug`;
2. an origin policy that serves extension origins
   (`chrome-extension://`, `moz-extension://`, `safari-web-extension://`) and
   clients that send no `Origin` at all (scripts, tests), and closes a connection
   whose `Origin` is a site.

A client that presents the wrong token is closed without a reply, so it learns
nothing from the refusal. Nothing is recorded before the greeting passes.

### Deliberately absent

The protocol carries no cookies and no request headers. The extension reports
*what* the browser asked for; replaying the browser's credentials would move them
into Gunda's process, its logs, and its job database. A download that needs a
session cookie is out of scope until there is a deliberate place to keep one.

### Limits

| Limit | Value | Where |
| --- | --- | --- |
| message size | 256 KiB | `protocol::MAX_MESSAGE_BYTES`, `websocket::MAX_MESSAGE_BYTES` |
| handshake size | 16 KiB | `websocket` |
| open connections | 8 | `server::MAX_CONNECTIONS` |
| time to handshake, time to greet | 10 s each | `server::GREETING_DEADLINE` |
| candidates held | 256 | `candidate::DEFAULT_CAPACITY` |
| reports held while disconnected | 64 | extension `background.js` |
| a URL is ignored for | 10 min | extension `background.js` |

### Evidence

The crate's 49 unit tests include the RFC 6455 handshake vector, framing
refusals, the token, the catalog, and the listener end to end over real loopback
sockets against a client that masks what a browser masks.

Against a foreign client, `cargo run -p gunda-capture --example listen` driven by
Node's (undici) `WebSocket`:

```
upgraded
answer {"kind":"ready","version":1}
answer {"kind":"accepted","id":0}          # stream.m3u8
answer {"kind":"duplicate","id":0}         # the same URL again
answer {"kind":"accepted","id":1}          # movie.mp4
answer {"kind":"refused","reason":"segment_stream"}   # seg-3.ts
closed 1005 ""
```

and the listener's own record:

```
id=0 kind=Hls  seen=2 url=https://cdn.example.com/stream.m3u8?token=abc page=https://example.com/watch?v=1 title=Some("Interop check")
id=1 kind=File seen=1 url=https://cdn.example.com/movie.mp4              page=https://example.com/watch?v=1 title=None
```

## 3.3 Two tracks into one file (`crates/gunda-stream::mux`)

The step that turns a split presentation into one file: a presentation whose
picture and sound are separate tracks becomes one container.
`mux_mp4(video, audio, output)` takes
the two staged track files and writes a single plain (non-fragmented) MP4 with
real sample tables, returning the size of what it wrote.

What the two files are is decided by their bytes, not by the manifest that named
them: three input shapes are read.

| Input | Read as |
| --- | --- |
| Fragmented MP4 — an initialization segment followed by one fragment per media segment (MPEG-DASH, HLS with `EXT-X-MAP`) | The ISO demuxer walks the boxes with seeks, reads the `trak` of the initialization segment and the `tfhd`/`tfdt`/`trun` of every fragment, and records where each sample's bytes are. |
| MPEG-TS — 188-byte packets (HLS without `EXT-X-MAP`, which is what YouTube serves when it pairs a TS variant with a TS audio rendition) | PAT/PMT section assembly, PES headers with PTS/DTS, then the elementary stream: H.264 access units found by AUD/VCL boundaries, ADTS frames with their headers stripped. |
| ADTS AAC — packed audio, with an ID3v2 tag in front of every segment | The same ADTS parser; each tag is stepped over whole, because it carries timing rather than audio. |

A transport stream is demuxed into a single elementary track and given a sample
entry built from what the elementary stream declares: an `avcC` from the SPS/PPS
and the picture size from the SPS, or an `AudioSpecificConfig` from the ADTS
header. Video timing uses a 90 kHz timescale, sample durations from decode-time
deltas (a discontinuity of zero or more than a second reuses the previous
duration rather than inventing a break); AAC samples are 1024 samples long at
the stream's own sample rate. Only H.264 video and AAC audio are read out of a
transport stream: HEVC, MPEG audio and Dolby audio are refused with a message
that says which was found, because carrying them over is not possible and
mislabeling them would be worse.

The writer reuses what it read:

| Carried over untouched | Written fresh |
| --- | --- |
| `stsd` (with `avcC`/`esds`/`av1C`/`vpcC`/`dOps`), `hdlr`, `dinf`, `vmhd`/`smhd`/`nmhd` | `ftyp`, `mvhd`, and every sample table: `stts`, `ctts` (only when reordered), `stsc`, `stsz`, `stss` (only when not every sample is a sync sample), `stco` |
| The sample bytes themselves, copied in order | `tkhd`/`mdhd` durations, with the track identifiers renumbered |

For ISO input, because the sample entry and its codec configuration boxes travel
verbatim, nothing there knows what a codec is: H.264, HEVC, AV1, VP9, AAC and
Opus all take the same path. Transport-stream input is necessarily narrower: only
H.264 and AAC have a synthesized sample entry, and the configuration is built
from the elementary stream itself. Each track's samples are written as one run,
so one chunk offset locates them; the offsets are filled in after the length of
the initialization data is known, which is why the box writer keeps one flat
buffer and records where each offset field sits. A gap between fragments is
absorbed by lengthening the sample before it (`stts` describes durations, not
absolute times); two fragments that overlap are refused rather than silently
dropping media.

Limits, all typed (`MuxError`) rather than approximated:

| Limit | Why |
| --- | --- |
| No 64-bit box sizes, no `co64`, output ≤ 4 GiB | The writer emits 32-bit sizes and chunk offsets. A larger file is refused, not truncated. |
| `UndeclaredSample` | A sample whose duration or size the fragments do not declare cannot be tabled. |
| Second sample entry refused | Only the first `stsd` entry is written, so a fragment that selects another one would be silently wrong. |
| A transport stream carries one track | The demuxer reads the packet stream's single elementary track. A file that holds both picture and sound is refused, so each track is staged on its own before muxing. |
| Transport streams limited to H.264 and AAC | HEVC (stream type `0x24`), MPEG audio (`0x03`/`0x04`) and Dolby (`0x81`/`0x87`) are refused with a typed message rather than mislabeled. |
| No Matroska writer yet | `.mkv` remains a later step; MP4 covers what YouTube serves (fMP4 with H.264/HEVC/AV1/VP9 + AAC/Opus). |
| No separate mux progress | Muxing runs after the last byte arrives and is not reported as its own phase; progress reaches the fetched total and then jumps to the muxed total. |

### Evidence that it produces a real file

Observed, on this machine, with a real presentation whose sound is a track of
its own (`https://storage.googleapis.com/shaka-demo-assets/angel-one-hls/hls.m3u8`,
H.264 768×576 + AAC, 60 s):

- The engine fetched it and published
  `target/mux-probe/hls.mp4` — 11,778,326 bytes, while the sum of the fetched
  resources was 11,791,362 (the difference is the initialization segments, which
  describe the tracks but carry no media).
- The Windows shell property handler (Media Foundation), which knows nothing
  about Gunda, reports: *MP4 Video File*, length `00:01:00`, frame height `576`,
  frame width `768`, frame rate `25.00`, video compression
  `{34363248-0000-0010-8000-00AA00389B71}` (the `H264` fourcc).
- WPF `MediaPlayer` (also Media Foundation) opened it and reports
  `HasVideo=True`, `HasAudio=True`, `768×576`, duration `00:01:00`; after five
  seconds of playback its position had advanced to `00:00:04.92`.

Conclusion (inference from those two observations): a foreign media stack parses
the container and its codec configuration and decodes **both** tracks. It is not
evidence that every frame is correct, that every codec YouTube serves is
covered, or that the Matroska path exists — none of those are claimed.

Observed, also on this machine, for transport-stream input: the Apple advanced
TS example
(`https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_ts/master.m3u8`,
which pairs a TS video variant with a separate audio rendition) was fetched for
three segments per track. The video variant was `v9` (1920×1080, 60 fps, H.264
in TS) and the audio rendition `a1` (AAC, delivered as packed ADTS segments,
each beginning with an ID3v2 tag).

- The three video segments measured 6,006,224 + 5,994,756 + 5,987,988 =
  17,988,968 bytes; the three audio segments 365,689 bytes.
- `mux_mp4` published `target/mux-probe/ts.mp4` at 17,945,972 bytes — smaller
  than the input because 188-byte packet headers and ID3 tags are not media.
- WPF `MediaPlayer` opened it and reports `HasVideo=True`, `HasAudio=True`,
  1920×1080, duration `00:00:18`; after four seconds of playback its position
  had advanced to `00:00:03.90`.

Conclusion (inference): the demuxer reads real transport-stream bytes from a
production encoder, including the ID3 tags packed-audio segments carry, and the
resulting file is decoded by Windows' own media stack.

Observed, finally, on the failing case itself: the HLS variant manifest that a
YouTube watch page had produced (the job the desktop app recorded as failed with
`separate audio and video tracks require muxing`; a 1920×1080 variant with a
separate audio rendition, 151.5 s) was prepared, fetched and published by the
same engine: `target/yt-probe/index.mp4`, 59,522,230 bytes, with no external
tool. WPF `MediaPlayer` reports `HasVideo=True`, `HasAudio=True`, 1920×1080,
duration `00:02:31`; after five seconds of playback its position had advanced to
`00:00:04.97`. The same URL had previously ended at the refusal above.

One master can offer the same picture with several audio groups (stereo AAC,
surround Dolby), and the larger audio track usually gives its variant the higher
bandwidth. Bandwidth alone would therefore choose sound that cannot be read out
of a transport stream. When a master names more than one audio group, the
planner takes the best variant whose `CODECS` attribute declares AAC; a single
group, or a master that declares no codecs, leaves bandwidth in charge. The
choice is a default, not a restriction — the picker still lists every variant.

## 4. Security rules

- Only `http` and `https` resources are ever planned; a manifest cannot steer
  the engine at `file:` or a custom protocol handler.
- Manifests are remote input: XML nesting, node count, and segment count are all
  capped (`MAX_DEPTH`, `MAX_NODES`, `MAX_SEGMENTS`).
- An unsubstituted or unknown `$identifier$` in a DASH template is an error, not
  a URL to try.
- Nothing from the capture side is trusted to name a file on disk; the existing
  `plan_destination`/`validate_filename` path decides output names.
- The capture listener binds loopback explicitly; there is no option to bind a
  routable address.
- Loopback is not a boundary, because every page in the browser can reach it. The
  pairing token and the extension-origin policy are what protect the channel, and
  a connection that fails either is closed without a reply.
- The extension reports a URL, a content type, and a page title only: never
  cookies, never request headers, never page content.

## 5. Roadmap

Done:

1. `crates/gunda-stream`: HLS and DASH parsing, planning, and execution through
   `DownloadExecutor`, including byte-range segments, with end-to-end HLS, DASH,
   and ranged downloads against a local server. Container recognition from the
   media bytes, output naming from the media, measured totals, and `inspect` for
   quality/track selection.
2. HTTP byte ranges: `HttpClient::open_byte_range`, and the shared validated
   name (`url_filename`) the stream engine names its output with.
3. `crates/gunda-capture`: the loopback listener, the RFC 6455 subset, the
   versioned JSON protocol, the pairing token, and the candidate catalog with
   deduplication, verified against a foreign client (Node `undici`).
4. `apps/extension`: an MV3 build that sniffs responses, scans the DOM, and
   reports candidates, with an options page for pairing.
5. Desktop wiring: the capture panel (pairing, captured list, per-candidate
   download and dismiss), the browser-origin job path, and the quality picker
   ("Qualities" → per-track download, named after the presentation).
6. Storage: browser-origin jobs persist, carrying `source_page_url` and
   `source_title`.
7. **Muxing:** `gunda-stream::mux` combines the two tracks of a split
   presentation into one plain MP4 (no external tool, no codec knowledge), the
   executor fetches each track into its own staging file and publishes the muxed
   result, and DASH adaptation sets and HLS variant+rendition pairs are planned
   as two tracks. Transport-stream tracks are demuxed first (H.264 video, AAC
   audio), including packed ADTS audio behind ID3v2 tags, so the split HLS that
   YouTube serves is muxed as well. Verified against real presentations with
   Windows' own media stack (section 3.3).

Next, in order:

1. **Track selection inside one manifest.** A DASH representation or an HLS
   variant's audio rendition is not separately addressable, so a chosen quality
   cannot yet be muxed with its own rendition — `inspect` offers the whole
   presentation instead, which downloads the best quality. Reaching the plan
   with the chosen variant (a field on `RequestContext`, carried through the
   browser-origin job) lifts that restriction.
2. **A Matroska writer** (`.mkv`, EBML with `vp09`/`opus`/`av1`), so a Matroska
   pair can be muxed as well; and HEVC in a transport stream, which is currently
   refused.
3. **64-bit boxes (`co64`)**, lifting the 4 GiB limit on muxed downloads.
4. Desktop: surface the per-download refusal reason.
5. Segmented resume: a segment-index checkpoint table following
   `SqliteHttpResumeStore`, then `supports_pause()` returns `true`.
6. Optional: AES-128 HLS decryption, live capture for
   `IncompletePresentation`, a Firefox manifest variant for the extension
   (`background.scripts` instead of a service worker).
