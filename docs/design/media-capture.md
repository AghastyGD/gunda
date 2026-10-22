# Media capture and segmented streaming

This document records the problem browser media capture solves, the design
decisions behind Gunda's capture pipeline, and the current behavior of the
`gunda-stream` engine.

## 1. What browser media capture has to solve

Capture design constraints:

- **Pages do not expose a downloadable file.** Media arrives as a manifest (HLS
  `.m3u8`, DASH `.mpd`, Smooth Streaming) followed by segments, or as one
  progressive response. Media URLs are built at runtime and are frequently
  signed per session, so page markup is not a reliable source.
- **Requests identify the media resource.** A capture pipeline observes page
  requests and classifies responses by MIME type rather than URL shape. Sites
  can hide media behind opaque query strings and change paths, but a player
  only accepts a manifest with the correct content type.
- **A presentation can contain multiple resources.** At most quality levels the
  picture and sound travel as independent tracks with separate URLs.
  Concatenating those downloads does not produce a playable result because a
  container must describe both tracks. Split presentations therefore require a
  muxing step.

### The shape of a capture pipeline

```
[ page world ]        [ isolated world ]      [ service worker ]      [ native ]
request instrumentation <-> DOM observation <-> response sniffing --> local service
patch XHR/fetch/MSE        geometry + events    MIME classification
                                        |
                          ws://127.0.0.1:PORT or native messaging
```

The pipeline has these responsibilities:

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
  reach the Gunda daemon either over a loopback WebSocket or through the
  browser's native-messaging bridge, which needs an installed host manifest and
  a helper executable.
- **A native overlay, if the UI is one.** A window drawn over a video cannot
  receive DOM input, so the content script has to report the element's
  rectangle, device-pixel ratio, visibility and clipped ancestor box, the
  window's inner size, and mirror mouse, keyboard, scroll, resize and blur
  events.

The page-response agent also reads bounded JSON from Instagram and Facebook.
Instagram `video_versions` and `video_url` fields identify progressive MP4
files. Facebook `browser_native_*_url` and `progressive_urls` fields provide the
same fallback. Partial responses and range-addressed URLs are not accepted as
standalone files.

### Engine requirements

Presentations that already contain muxed media are downloaded directly. Split
audio and video require a container that describes both tracks, so Gunda muxes
them in-process without an external tool or subprocess.

## 2. Design decisions

1. **The browser side pushes to a local listener.** `gundad` listens on
   loopback and the extension delivers detected media candidates to it. The
   daemon decides what a candidate becomes.
2. **Detection keys off MIME type, not URL shape.** The manifest types Gunda
   recognises (`application/vnd.apple.mpegurl`, `application/dash+xml`,
   `application/vnd.yt-ump`, `application/octet-stream-m3u8`, and similar
   manifest types) are what make detection work across sites without per-site
   code.
3. **Correlation by request tagging.** The extension marks requests it can see
   so a detected element can be tied to the response that carries it.
4. **Segmented download is part of the engine, not a parallel lifecycle.** It
   plugs into the existing `DownloadExecutor` seam
   (`crates/gunda-core/src/application/execution.rs`). `ResourceKind::Hls` /
   `ResourceKind::Dash` and `DownloadOrigin::Browser { page_url, page_title }`
   already exist in the domain for exactly this.

Design choices:

- **No per-site player adapters.** Site-specific selectors require ongoing
  maintenance and break whenever a player changes. Gunda relies on
  manifest MIME detection and the reported `<video>`/MSE sources instead.
- **JSON over WebSocket, not a bespoke binary framing.** There is no legacy
  client to stay compatible with, and a self-describing protocol is easier to
  audit.
- **An authenticated loopback port.** Gunda requires a shared secret,
  established at extension-pairing time, on every connection and every message,
  so a page cannot inject downloads into the application.

## 3. The segmented download engine (`crates/gunda-stream`)

Implemented components:

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
  `http`/`https` resources with optional byte ranges. Initialization segments
  are interleaved where they apply, and the planner refuses a plan it cannot
  execute. A presentation that keeps its sound in a track of its own is planned
  as two tracks. Every resource carries the track it belongs to
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
  the partial file (section 3.3); those staging files are removed after a
  successful publication, and kept when a paused transfer can resume. Progress
  is reported as bytes are appended, a new attempt creates staging exclusively,
  resume opens it and truncates any uncheckpointed suffix, cancellation is
  handled while a resource body is arriving and between resources, and
  publication goes through `finalize_download` so filename validation and
  conflict policy are shared with plain HTTP downloads rather than
  reimplemented.
- `mux`: a fragmented-MP4 demuxer and a plain-MP4 writer, with no codec
  knowledge and no external tool. See section 3.3.
- `container`: the media's container is read from the media itself. The first
  planned resource is read up to `SNIFF_BYTES` (2048) and recognised from its
  leading bytes: ISO base media by brand and by its `hdlr` track handler
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
  downloading any media. For HLS it describes one track per
  `EXT-X-STREAM-INF` variant, each with the rendition the presentation pairs
  with it, plus each `TYPE=AUDIO` rendition that has a playlist of its own. A
  variant whose sound is a separate rendition is therefore still offered at
  every quality: its reported size and duration combine both halves, and the
  download carries a selector rather than the variant's own playlist URL. For
  DASH it describes every representation of the chosen video adaptation set
  with the default audio representation, plus audio-only tracks. Describing an
  HLS variant costs one request for its playlist, so the number described is
  capped (`MAX_DESCRIBED_VARIANTS`).

Naming and size rules:

- **The output is named after the presentation, not the manifest.** Captured
  downloads use the page title when it contains usable text. A shared sanitizer
  replaces unsafe filename characters, removes common browser annotations, and
  applies the media extension. Manifest and variant URLs frequently end in
  `index.m3u8`, so `output_filename` remains only a fallback. A filename the
  caller chose still wins, and `plan_destination` remains the only validator.
- **Reported sizes are measured or unknown.** A declared length is used
  directly. Otherwise every planned resource is probed with one bounded metadata
  request. The probe tries `HEAD`, then `Range: bytes=0-0` to read the total
  from `Content-Range`, then response metadata from a normal `GET`. At most
  `PROBE_CONCURRENCY` = 8 probes run in flight and at most
  `MAX_PROBED_RESOURCES` = 512 resources are accepted. Beyond that bound, or
  when the origin exposes no length through any method, the total is unknown.
  The picker may show an estimate from the declared bitrate and duration,
  marked with `~`.

Unsupported cases return named errors (`UnsupportedFeature`):

| Feature | Why it is refused |
| --- | --- |
| `EncryptedSegments` (`EXT-X-KEY` ≠ `NONE`) | Decryption is not implemented; publishing ciphertext as a finished video would be wrong. |
| `IncompletePresentation` | Live playlists and `type="dynamic"` MPDs would produce a silently truncated file. Continuous capture is a separate mode. |
| `SeparateTracks` | Split tracks whose bytes are not a container this can mux, such as Matroska/WebM input, cannot be combined. Publishing only the picture would lose the sound. ISO base media is carried over; MPEG-TS and ADTS audio are demuxed first. |
| `MultiPeriod` | Stitching periods requires re-encode decisions. |
| `SegmentIndex` | `SegmentBase`/`sidx` enumeration is not implemented. |
| `DataUriSegments` | Inline segments are not fetchable resources. |

Because these refusals are typed, the UI can explain the reason per download
instead of showing a generic failure.

Execution limits:

- **Resume is segment-boundary only.** The durable checkpoint records how many
  resources of each track are complete. Pausing inside a segment truncates its
  uncheckpointed bytes, so that one segment is fetched again. The plan
  fingerprint ignores common signing query parameters so rotating signatures do
  not invalidate a checkpoint; a changed path, non-signature query, byte
  interval, or track count is refused rather than resumed.

## 3.1 Seams added to `gunda-http`

The streaming engine uses four HTTP operations:

- `PartialDownload::adopt`: adopts the staging file the engine wrote, so
  publication still runs through `finalize_download` (validated filename,
  conflict policy, partial-file identity).
- `plan_destination`: shares destination naming policy, so an engine that
  planned a different final path than the manager would not report the
  wrong result to the user.
- `HttpInspection::staged`: constructs a staging file with no
  remote validator.
- `HttpClient::open_byte_range`: fetches exactly `bytes=first-last` with no
  validator. The existing `open_range` cannot serve this: it resumes from an
  offset to the end of a representation and requires a strong entity tag. A
  server that ignores `Range` and answers `200 OK` with the whole resource is
  rejected, because appending a whole file where one segment was asked for
  would corrupt the output.

## 3.2 The capture channel (`crates/gunda-capture`, `apps/extension`)

The Gunda daemon runs a loopback WebSocket listener because it requires no
native host registration or extra executable:

```
page -- runtime messages --> extension service worker -- WebSocket --> CaptureServer
       (content.js)             (background.js)           ws://127.0.0.1:PORT/capture
```

### Transport

The RFC 6455 subset needed to accept one browser connection is implemented in
`crates/gunda-capture/src/websocket.rs`: the opening handshake, text messages
(fragmented or not), ping/pong, and close. Binary messages, reserved opcodes, and
negotiated extensions are refused rather than ignored. The implementation keeps
a small dependency set:

- no WebSocket crate is in `Cargo.lock`, so a dependency would have to be
  downloaded, and the workspace keeps its dependency set small on purpose;
- the subset needed by a single-client, text-only channel is bounded;
- the handshake follows the worked example in RFC 6455 section 1.3.

A frame that cannot be understood ends the connection; nothing is dropped
silently. Client frames must be masked, control frames must be short and final,
payloads are capped, and the reserved bits must be clear.

### Protocol

JSON objects tagged with a `kind` (`serde` internally tagged enums), versioned by
the greeting, limited to `MAX_MESSAGE_BYTES` (256 KiB).

| Extension to desktop | Payload |
| --- | --- |
| `hello` | `version`, `token`, `agent`; the first message on every connection |
| `candidate` | `url`, `content_type?`, `page_url`, `page_title?` |

| Desktop to extension | Meaning |
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

### Request credentials

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

## 3.3 Two tracks into one file (`crates/gunda-stream::mux`)

`mux_mp4(video, audio, output)` combines two staged track files into one plain,
non-fragmented MP4 with sample tables. It returns the size of the written file.

The input files are detected from their bytes rather than the manifest. Three
input shapes are supported.

| Input | Read as |
| --- | --- |
| Fragmented MP4: an initialization segment followed by one fragment per media segment (MPEG-DASH, HLS with `EXT-X-MAP`) | The ISO demuxer walks the boxes with seeks, reads the `trak` of the initialization segment and the `tfhd`/`tfdt`/`trun` of every fragment, and records where each sample's bytes are. |
| MPEG-TS: 188-byte packets (HLS without `EXT-X-MAP`, which is what YouTube serves when it pairs a TS variant with a TS audio rendition) | PAT/PMT section assembly, PES headers with PTS/DTS, then the elementary stream: H.264 access units found by AUD/VCL boundaries, ADTS frames with their headers stripped. |
| ADTS AAC: packed audio with an ID3v2 tag in front of every segment | The same ADTS parser; each tag is stepped over whole, because it carries timing rather than audio. |

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

The writer reuses and generates these boxes:

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

Mux limits return typed errors (`MuxError`):

| Limit | Why |
| --- | --- |
| Metadata boxes remain 32-bit | Sample offsets use `co64` and `mdat` uses its extended size when needed; the movie and track metadata boxes themselves still use 32-bit sizes. |
| `UndeclaredSample` | A sample whose duration or size the fragments do not declare cannot be tabled. |
| Second sample entry refused | Only the first `stsd` entry is written, so a fragment that selects another one would be silently wrong. |
| A transport stream carries one track | The demuxer reads the packet stream's single elementary track. A file that holds both picture and sound is refused, so each track is staged on its own before muxing. |
| Transport streams limited to H.264 and AAC | HEVC (stream type `0x24`), MPEG audio (`0x03`/`0x04`) and Dolby (`0x81`/`0x87`) are refused with a typed message rather than mislabeled. |
| No Matroska writer yet | `.mkv` remains a later step; MP4 covers what YouTube serves (fMP4 with H.264/HEVC/AV1/VP9 + AAC/Opus). |
| No separate mux progress | Muxing runs after the last byte arrives and is not reported as its own phase; progress counts fetched bytes, while the published file can be smaller because initialization data and container headers are not media. |

One master can offer the same picture with several audio groups (stereo AAC,
surround Dolby), and the larger audio track usually gives its variant the higher
bandwidth. Bandwidth alone would therefore choose sound that cannot be read out
of a transport stream. When a master names more than one audio group, the
planner takes the best variant whose `CODECS` attribute declares AAC; a single
group, or a master that declares no codecs, leaves bandwidth in charge. The
choice is a default, not a restriction; the picker still lists every variant.

## 4. Security rules

- Only `http` and `https` resources are ever planned; a manifest cannot steer
  the engine at `file:` or a custom protocol handler.
- Manifests are remote input: XML nesting, node count, and segment count are all
  capped (`MAX_DEPTH`, `MAX_NODES`, `MAX_SEGMENTS`).
- An unsubstituted or unknown `$identifier$` in a DASH template is an error, not
  a URL to try.
- Nothing from the capture side is trusted to name a file on disk; the existing
  `plan_destination`/`validate_filename` path decides output names.
- The capture listener in `gundad` binds loopback explicitly; there is no option
  to bind a routable address.
- Loopback is not a boundary, because every page in the browser can reach it. The
  pairing token and the extension-origin policy are what protect the channel, and
  a connection that fails either is closed without a reply.
- The extension reports a URL, a content type, and a page title only: never
  cookies, never request headers, never page content.

## 5. Roadmap

Implemented:

1. `crates/gunda-stream`: HLS and DASH parsing, planning, and execution through
   `DownloadExecutor`, including byte-range segments, container recognition
   from the media bytes, output naming from the media, measured totals, and
   `inspect` for quality/track selection.
2. HTTP byte ranges: `HttpClient::open_byte_range`, and the shared validated
   name (`url_filename`) the stream engine names its output with.
3. `crates/gunda-capture`: the loopback listener, the RFC 6455 subset, the
   versioned JSON protocol, the pairing token, and the candidate catalog with
   deduplication.
4. `apps/extension`: an MV3 build that sniffs responses, scans the DOM, reads
   bounded page JSON for progressive media, and reports candidates, with an
   options page for pairing.
5. Daemon and desktop wiring: `gundad` owns capture and quality inspection, and
   the desktop panel shows pairing, candidates, per-candidate download and
   dismiss actions, and the quality picker.
6. Storage: browser-origin jobs persist, carrying `source_page_url` and
   `source_title`.
7. **Muxing:** `gunda-stream::mux` combines the two tracks of a split
   presentation into one plain MP4 (no external tool, no codec knowledge), the
   executor fetches each track into its own staging file and publishes the muxed
   result, and DASH adaptation sets and HLS variant+rendition pairs are planned
   as two tracks. Transport-stream tracks are demuxed first (H.264 video, AAC
   audio), including packed ADTS audio behind ID3v2 tags, so the split HLS that
   YouTube serves is muxed as well.
8. **Track selection inside one manifest:** `inspect` describes each HLS
   variant with its paired audio rendition and each DASH representation with
   its audio set. The choice is persisted as a small, URL-free
   `ExecutionSelection` on the request and re-resolved when the job executes.
9. **Segmented resume:** `stream_resume_state` and `stream_resume_tracks`
   persist the plan fingerprint and each track's next resource and durable byte
   count. Pause keeps staging, cancels and restarts remove it, and resumed
   downloads truncate any bytes beyond the last checkpoint before continuing.

Planned:

1. **A Matroska writer** (`.mkv`, EBML with `vp09`/`opus`/`av1`), so a Matroska
   pair can be muxed as well; and HEVC in a transport stream, which is currently
   refused.
2. Desktop: surface the per-download refusal reason.
3. Optional: AES-128 HLS decryption, live capture for
   `IncompletePresentation`, a Firefox manifest variant for the extension
   (`background.scripts` instead of a service worker).
