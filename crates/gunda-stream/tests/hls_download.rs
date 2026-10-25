//! End-to-end execution of a segmented plan against a local server.

use std::time::Duration;

use gunda_core::application::{
    DownloadExecutor, ExecutionInput, PreparedTransfer, StagedTransfer, TransferOutcome,
};
use gunda_core::download::{
    DownloadDestination, DownloadId, DownloadProgress, FileConflictPolicy, RequestContext,
    ResourceKind,
};
use gunda_http::HttpClient;
use gunda_stream::{StreamExecutor, TrackKind, TrackSize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use url::Url;

const MASTER: &str = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=1200000,RESOLUTION=1280x720\n\
media.m3u8\n";

const MEDIA: &str = "#EXTM3U\n\
#EXT-X-MAP:URI=\"init.mp4\"\n\
#EXTINF:4,\n\
s0.m4s\n\
#EXTINF:4,\n\
s1.m4s\n\
#EXTINF:4,\n\
s2.m4s\n\
#EXT-X-ENDLIST\n";

/// Two sub-ranges of one transport stream. The second omits its offset, so the
/// plan must continue after the first sub-range.
const RANGED_HLS: &str = "#EXTM3U\n\
#EXTINF:4,\n\
#EXT-X-BYTERANGE:4@0\n\
all.ts\n\
#EXTINF:4,\n\
#EXT-X-BYTERANGE:4\n\
all.ts\n\
#EXT-X-ENDLIST\n";

/// A master playlist offering two video qualities and one audio-only track.
const VARIANT_MASTER: &str = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360,AUDIO=\"aac\"\n\
low.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,AUDIO=\"aac\"\n\
high.m3u8\n";

/// A master playlist whose variants carry their own sound. The rendition is
/// only declared, with no playlist of its own, so it is inside the variants.
const MUXED_MASTER: &str = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\n\
low.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\n\
high.m3u8\n";

/// A variant that declares the length of every part, so its size is exact.
const RANGED_VARIANT: &str = "#EXTM3U\n\
#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"4@0\"\n\
#EXTINF:4,\n\
#EXT-X-BYTERANGE:4@4\n\
low.m4s\n\
#EXTINF:4,\n\
#EXT-X-BYTERANGE:4@8\n\
low.m4s\n\
#EXT-X-ENDLIST\n";

/// A variant that declares no lengths, so its size can only be estimated.
const PLAIN_VARIANT: &str = "#EXTM3U\n\
#EXT-X-MAP:URI=\"init.mp4\"\n\
#EXTINF:4,\n\
high0.m4s\n\
#EXTINF:4,\n\
high1.m4s\n\
#EXT-X-ENDLIST\n";

/// An audio-only rendition of the same presentation.
const AUDIO_RENDITION: &str = "#EXTM3U\n\
#EXTINF:4,\n\
audio.aac\n\
#EXT-X-ENDLIST\n";

const RANGED_DASH: &str = r#"<MPD type="static">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v0" bandwidth="1">
        <BaseURL>movie.mp4</BaseURL>
        <SegmentList>
          <Initialization sourceURL="movie.mp4" range="0-3"/>
          <SegmentURL media="movie.mp4" mediaRange="4-7"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

const MULTI_DASH: &str = r#"<MPD type="static" mediaPresentationDuration="PT4S">
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
</MPD>"#;

/// Serves fixed bodies by request path, honouring `Range`, and closes each
/// connection after one response.
async fn serve(routes: &[(&str, &str, &[u8])], entry: &str) -> Url {
    serve_with_head(routes, entry, true).await
}

async fn serve_with_head(routes: &[(&str, &str, &[u8])], entry: &str, allow_head: bool) -> Url {
    let routes: Vec<(String, String, Vec<u8>)> = routes
        .iter()
        .map(|(path, content_type, body)| {
            (
                (*path).to_owned(),
                (*content_type).to_owned(),
                body.to_vec(),
            )
        })
        .collect();

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("local listener must bind");

    let address = listener.local_addr().expect("listener address must exist");

    tokio::spawn(async move {
        let _ = timeout(Duration::from_secs(10), async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };

                let routes = routes.clone();

                tokio::spawn(async move {
                    let mut buffer = vec![0_u8; 8192];
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();

                    let response = if !allow_head && head.starts_with("HEAD ") {
                        b"HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\
                          connection: close\r\n\r\n"
                            .to_vec()
                    } else {
                        match routes.iter().find(|(route, _, _)| route == &path) {
                            Some((_, content_type, body)) => match requested_range(&head) {
                                Some((first, last)) if last < body.len() as u64 => {
                                    let slice = &body[first as usize..=last as usize];

                                    let mut response = format!(
                                        "HTTP/1.1 206 Partial Content\r\n\
                                     content-type: {content_type}\r\n\
                                     content-range: bytes {first}-{last}/{}\r\n\
                                     content-length: {}\r\n\
                                     connection: close\r\n\r\n",
                                        body.len(),
                                        slice.len()
                                    )
                                    .into_bytes();

                                    response.extend_from_slice(slice);
                                    response
                                }
                                _ => {
                                    let mut response = format!(
                                        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\n\
                                     content-length: {}\r\nconnection: close\r\n\r\n",
                                        body.len()
                                    )
                                    .into_bytes();

                                    response.extend_from_slice(body);
                                    response
                                }
                            },
                            None => b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\
                                      connection: close\r\n\r\n"
                                .to_vec(),
                        }
                    };

                    let _ = socket.write_all(&response).await;
                    let _ = socket.flush().await;
                });
            }
        })
        .await;
    });

    Url::parse(&format!("http://{address}{entry}")).expect("entry URL must parse")
}

/// Reads an `Range: bytes=first-last` header from a request head.
fn requested_range(head: &str) -> Option<(u64, u64)> {
    let value = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;

        if name.trim().eq_ignore_ascii_case("range") {
            Some(value.trim())
        } else {
            None
        }
    })?;

    let (first, last) = value.strip_prefix("bytes=")?.split_once('-')?;

    Some((first.parse().ok()?, last.parse().ok()?))
}

fn input(url: Url, directory: &std::path::Path) -> ExecutionInput {
    input_named(url, directory, Some("video.mp4".to_owned()))
}

/// Builds an execution whose destination name the caller left open.
fn input_unnamed(url: Url, directory: &std::path::Path) -> ExecutionInput {
    input_named(url, directory, None)
}

fn input_named(url: Url, directory: &std::path::Path, filename: Option<String>) -> ExecutionInput {
    ExecutionInput {
        id: DownloadId::new(1).expect("ID must be valid"),
        request: RequestContext::new(url, Vec::new()),
        destination: DownloadDestination::new(
            directory.to_path_buf(),
            filename,
            FileConflictPolicy::Fail,
        ),
        progress: DownloadProgress::default(),
        resource: None,
        resolved_destination: None,
    }
}

/// Downloads a playlist, then publishes it, returning the published bytes.
async fn download(entry: Url, directory: &std::path::Path) -> Vec<u8> {
    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));

    let prepared = executor
        .prepare(input(entry, directory))
        .await
        .expect("prepare must succeed");

    let outcome = prepared.transfer().await.expect("transfer must succeed");

    let TransferOutcome::Finished(staged) = outcome else {
        panic!("the transfer must finish");
    };

    let output = staged.finalize().await.expect("finalize must succeed");

    assert!(!output.cleanup_pending);

    let bytes = std::fs::read(output.destination.final_path()).expect("output must be readable");

    assert_eq!(
        u64::try_from(bytes.len()).expect("length must fit"),
        output.written_bytes
    );

    bytes
}

#[tokio::test]
async fn downloads_an_hls_presentation_into_one_file() {
    let master = serve(
        &[
            (
                "/master.m3u8",
                "application/vnd.apple.mpegurl",
                MASTER.as_bytes(),
            ),
            (
                "/media.m3u8",
                "application/vnd.apple.mpegurl",
                MEDIA.as_bytes(),
            ),
            ("/init.mp4", "video/mp4", b"INIT"),
            ("/s0.m4s", "video/mp4", b"SEG0"),
            ("/s1.m4s", "video/mp4", b"SEG1"),
            ("/s2.m4s", "video/mp4", b"SEG2"),
        ],
        "/master.m3u8",
    )
    .await;

    let directory = tempfile::tempdir().expect("temp directory must exist");
    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));

    let prepared = executor
        .prepare(input(master, directory.path()))
        .await
        .expect("prepare must succeed");

    assert_eq!(prepared.resource().kind(), ResourceKind::Hls);
    // The playlist declares how long each segment plays, not how large it is,
    // so the sizes are measured from the origin before the transfer starts.
    assert_eq!(prepared.total_bytes(), Some(16));

    let outcome = prepared.transfer().await.expect("transfer must succeed");

    let TransferOutcome::Finished(staged) = outcome else {
        panic!("the transfer must finish");
    };

    assert_eq!(staged.written_bytes(), 16);

    let output = staged.finalize().await.expect("finalize must succeed");

    assert_eq!(output.written_bytes, 16);

    let bytes = std::fs::read(output.destination.final_path()).expect("output must be readable");

    // The plan order is initialization segment first, then media segments.
    assert_eq!(bytes, b"INITSEG0SEG1SEG2");

    // Staging is cleaned up by publication.
    let leftovers: Vec<_> = std::fs::read_dir(directory.path())
        .expect("directory must be readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".part"))
        .collect();

    assert!(
        leftovers.is_empty(),
        "unexpected staging files: {leftovers:?}"
    );
}

#[tokio::test]
async fn a_server_that_rejects_head_still_reports_the_measured_size() {
    let master = serve_with_head(
        &[
            (
                "/master.m3u8",
                "application/vnd.apple.mpegurl",
                MASTER.as_bytes(),
            ),
            (
                "/media.m3u8",
                "application/vnd.apple.mpegurl",
                MEDIA.as_bytes(),
            ),
            ("/init.mp4", "video/mp4", b"INIT"),
            ("/s0.m4s", "video/mp4", b"SEG0"),
            ("/s1.m4s", "video/mp4", b"SEG1"),
            ("/s2.m4s", "video/mp4", b"SEG2"),
        ],
        "/master.m3u8",
        false,
    )
    .await;

    let directory = tempfile::tempdir().expect("temp directory must exist");
    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));
    let prepared = executor
        .prepare(input(master, directory.path()))
        .await
        .expect("prepare must succeed");

    assert_eq!(prepared.total_bytes(), Some(16));
}

#[tokio::test]
async fn downloads_hls_byte_ranges_of_one_file() {
    let entry = serve(
        &[
            (
                "/media.m3u8",
                "application/vnd.apple.mpegurl",
                RANGED_HLS.as_bytes(),
            ),
            ("/all.ts", "video/mp2t", b"HEADTAIL"),
        ],
        "/media.m3u8",
    )
    .await;

    let directory = tempfile::tempdir().expect("temp directory must exist");

    // Each sub-range must be fetched as a range: reading the whole resource
    // twice would produce sixteen bytes instead of eight.
    assert_eq!(download(entry, directory.path()).await, b"HEADTAIL");
}

#[tokio::test]
async fn inspection_describes_a_presentation_whose_qualities_share_one_rendition() {
    let entry = serve(
        &[
            (
                "/master.m3u8",
                "application/vnd.apple.mpegurl",
                VARIANT_MASTER.as_bytes(),
            ),
            (
                "/low.m3u8",
                "application/vnd.apple.mpegurl",
                RANGED_VARIANT.as_bytes(),
            ),
            (
                "/high.m3u8",
                "application/vnd.apple.mpegurl",
                PLAIN_VARIANT.as_bytes(),
            ),
            (
                "/audio.m3u8",
                "application/vnd.apple.mpegurl",
                AUDIO_RENDITION.as_bytes(),
            ),
            ("/init.mp4", "video/mp4", b"INIT"),
            ("/low.m4s", "video/mp4", b"HEADTAIL"),
            ("/high0.m4s", "video/mp4", b"SEG0"),
            ("/high1.m4s", "video/mp4", b"SEG1"),
            ("/audio.aac", "audio/aac", b"AAC0"),
        ],
        "/master.m3u8",
    )
    .await;

    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));

    let presentation = executor
        .inspect(&RequestContext::new(entry.clone(), Vec::new()))
        .await
        .expect("inspection must succeed");

    let tracks = presentation.tracks();

    // Each quality is offered together with the rendition the presentation
    // pairs it with, so choosing a lower quality cannot produce a silent file.
    assert_eq!(tracks.len(), 3);

    assert_eq!(tracks[0].label(), "1920x1080 (5.0 Mbps) with audio");
    assert_eq!(tracks[0].kind(), TrackKind::Video);
    assert_eq!(tracks[0].url(), &entry);
    assert_eq!(tracks[0].resolution(), Some((1920, 1080)));
    assert_eq!(tracks[0].duration_millis(), None);
    assert_eq!(tracks[0].container(), None);
    assert_eq!(tracks[0].filename(), None);
    assert!(tracks[0].selection().is_some());

    // Neither size is declared in full, so the whole is not reported as exact.
    assert_eq!(tracks[0].size(), TrackSize::Unknown);

    assert_eq!(tracks[1].label(), "640x360 (1.0 Mbps) with audio");
    assert_eq!(tracks[1].kind(), TrackKind::Video);
    assert_eq!(tracks[1].url(), &entry);
    assert_eq!(tracks[1].resolution(), Some((640, 360)));
    assert_eq!(tracks[1].duration_millis(), None);
    assert_eq!(tracks[1].container(), None);
    assert_eq!(tracks[1].filename(), None);
    assert!(tracks[1].selection().is_some());
    assert_eq!(tracks[1].size(), TrackSize::Unknown);

    // An audio-only rendition is separately addressable, so it stays a track of
    // its own, and is still named after the presentation rather than after its
    // own playlist.
    assert_eq!(tracks[2].label(), "English (en)");
    assert_eq!(tracks[2].kind(), TrackKind::Audio);
    assert_eq!(tracks[2].size(), TrackSize::Unknown);
    assert_eq!(tracks[2].duration_millis(), None);
    assert_eq!(tracks[2].filename(), None);
}

#[tokio::test]
async fn inspection_describes_every_quality_of_a_presentation_that_carries_its_sound() {
    let entry = serve(
        &[
            (
                "/master.m3u8",
                "application/vnd.apple.mpegurl",
                MUXED_MASTER.as_bytes(),
            ),
            (
                "/low.m3u8",
                "application/vnd.apple.mpegurl",
                RANGED_VARIANT.as_bytes(),
            ),
            (
                "/high.m3u8",
                "application/vnd.apple.mpegurl",
                PLAIN_VARIANT.as_bytes(),
            ),
            ("/init.mp4", "video/mp4", b"INIT"),
            ("/low.m4s", "video/mp4", b"HEADTAIL"),
            ("/high0.m4s", "video/mp4", b"SEG0"),
            ("/high1.m4s", "video/mp4", b"SEG1"),
        ],
        "/master.m3u8",
    )
    .await;

    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));

    let presentation = executor
        .inspect(&RequestContext::new(entry, Vec::new()))
        .await
        .expect("inspection must succeed");

    let tracks = presentation.tracks();

    // Every variant carries its own sound and can be downloaded on its own, so
    // every quality is offered; the rendition that is only declared, with no
    // playlist of its own, is inside them and is not a track.
    assert_eq!(tracks.len(), 2);

    assert_eq!(tracks[0].label(), "1920x1080 (5.0 Mbps)");
    assert_eq!(tracks[0].kind(), TrackKind::Video);
    assert_eq!(tracks[0].size(), TrackSize::Unknown);
    assert_eq!(tracks[0].container(), None);
    assert_eq!(tracks[0].duration_millis(), None);
    assert_eq!(tracks[0].filename(), None);

    // The variant that declares every part's length is exactly known.
    assert_eq!(tracks[1].label(), "640x360 (1.0 Mbps)");
    assert_eq!(tracks[1].size(), TrackSize::Unknown);
    assert_eq!(tracks[1].container(), None);
    assert_eq!(tracks[1].duration_millis(), None);
    assert_eq!(tracks[1].filename(), None);
}

#[tokio::test]
async fn inspection_of_a_single_playlist_is_one_track() {
    let entry = serve(
        &[
            (
                "/media.m3u8",
                "application/vnd.apple.mpegurl",
                MEDIA.as_bytes(),
            ),
            ("/init.mp4", "video/mp4", b"INIT"),
            ("/s0.m4s", "video/mp4", b"SEG0"),
            ("/s1.m4s", "video/mp4", b"SEG1"),
            ("/s2.m4s", "video/mp4", b"SEG2"),
        ],
        "/media.m3u8",
    )
    .await;

    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));

    let presentation = executor
        .inspect(&RequestContext::new(entry.clone(), Vec::new()))
        .await
        .expect("inspection must succeed");

    let tracks = presentation.tracks();

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].label(), "Video");
    assert_eq!(tracks[0].kind(), TrackKind::Video);
    assert_eq!(tracks[0].url(), &entry);
    assert_eq!(tracks[0].duration_millis(), Some(12000));
    assert_eq!(tracks[0].filename(), Some("media.mp4"));
}

#[tokio::test]
async fn inspection_lists_every_dash_video_quality_with_its_audio() {
    let entry = serve(
        &[(
            "/manifest.mpd",
            "application/dash+xml",
            MULTI_DASH.as_bytes(),
        )],
        "/manifest.mpd",
    )
    .await;
    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));

    let presentation = executor
        .inspect(&RequestContext::new(entry.clone(), Vec::new()))
        .await
        .expect("inspection must succeed");
    let tracks = presentation.tracks();

    assert_eq!(tracks.len(), 3);
    assert_eq!(tracks[0].label(), "1920x1080 (v1) with audio");
    assert_eq!(tracks[0].url(), &entry);
    assert!(tracks[0].selection().is_some());
    assert_eq!(tracks[0].resolution(), Some((1920, 1080)));
    assert_eq!(tracks[0].size(), TrackSize::Estimated(1_264_000));

    assert_eq!(tracks[1].label(), "640x360 (v0) with audio");
    assert!(tracks[1].selection().is_some());
    assert_eq!(tracks[1].resolution(), Some((640, 360)));
    assert_eq!(tracks[1].size(), TrackSize::Estimated(464_000));

    // The audio is also addressable on its own.
    assert_eq!(tracks[2].label(), "Audio (en) · 128 kbps");
    assert_eq!(tracks[2].kind(), TrackKind::Audio);
    assert!(tracks[2].selection().is_some());
}

#[tokio::test]
async fn the_output_is_named_after_the_media_the_playlist_carries() {
    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));
    let directory = tempfile::tempdir().expect("temp directory must exist");

    // `EXT-X-MAP` carries an initialization segment, so these segments are
    // fragmented MP4 even though the entry point is named `master.m3u8`.
    let fragmented = serve(
        &[
            (
                "/master.m3u8",
                "application/vnd.apple.mpegurl",
                MASTER.as_bytes(),
            ),
            (
                "/media.m3u8",
                "application/vnd.apple.mpegurl",
                MEDIA.as_bytes(),
            ),
            ("/init.mp4", "video/mp4", b"INIT"),
            ("/s0.m4s", "video/mp4", b"SEG0"),
            ("/s1.m4s", "video/mp4", b"SEG1"),
            ("/s2.m4s", "video/mp4", b"SEG2"),
        ],
        "/master.m3u8",
    )
    .await;

    let prepared = executor
        .prepare(input_unnamed(fragmented, directory.path()))
        .await
        .expect("prepare must succeed");

    assert_eq!(
        prepared.planned_destination().final_path(),
        directory.path().join("master.mp4").as_path()
    );

    // A playlist without an initialization segment carries transport-stream
    // packets, whatever the URL is called.
    let packets = serve(
        &[
            (
                "/media.m3u8",
                "application/vnd.apple.mpegurl",
                RANGED_HLS.as_bytes(),
            ),
            ("/all.ts", "video/mp2t", b"HEADTAIL"),
        ],
        "/media.m3u8",
    )
    .await;

    let prepared = executor
        .prepare(input_unnamed(packets, directory.path()))
        .await
        .expect("prepare must succeed");

    assert_eq!(
        prepared.planned_destination().final_path(),
        directory.path().join("media.ts").as_path()
    );
}

#[tokio::test]
async fn downloads_dash_byte_ranges_of_one_file() {
    let entry = serve(
        &[
            (
                "/manifest.mpd",
                "application/dash+xml",
                RANGED_DASH.as_bytes(),
            ),
            ("/movie.mp4", "video/mp4", b"INITSEG0"),
        ],
        "/manifest.mpd",
    )
    .await;

    let directory = tempfile::tempdir().expect("temp directory must exist");
    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));

    let prepared = executor
        .prepare(input(entry, directory.path()))
        .await
        .expect("prepare must succeed");

    assert_eq!(prepared.resource().kind(), ResourceKind::Dash);
    // Every resource of this plan declares a length, so the total is known.
    assert_eq!(prepared.total_bytes(), Some(8));

    let outcome = prepared.transfer().await.expect("transfer must succeed");

    let TransferOutcome::Finished(staged) = outcome else {
        panic!("the transfer must finish");
    };

    let output = staged.finalize().await.expect("finalize must succeed");

    let bytes = std::fs::read(output.destination.final_path()).expect("output must be readable");

    assert_eq!(bytes, b"INITSEG0");
}

// The fixtures below are a fragmented MP4 presentation, reduced to what the
// format requires. The muxer carries the sample entry over untouched and never
// reads codec data, so the samples are a byte pattern and the sample entry is a
// placeholder of the right shape.

/// A master playlist whose variant carries no audio of its own.
const SPLIT_MASTER: &str = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360,AUDIO=\"aac\"\n\
video.m3u8\n";

const SPLIT_VIDEO: &str = "#EXTM3U\n\
#EXT-X-MAP:URI=\"v-init.mp4\"\n\
#EXTINF:4,\n\
v0.m4s\n\
#EXTINF:4,\n\
v1.m4s\n\
#EXT-X-ENDLIST\n";

const SPLIT_AUDIO: &str = "#EXTM3U\n\
#EXT-X-MAP:URI=\"a-init.mp4\"\n\
#EXTINF:4,\n\
a0.m4s\n\
#EXT-X-ENDLIST\n";

/// One complete box, carrying its payload.
fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut payload = vec![version];
    payload.extend_from_slice(&flags.to_be_bytes()[1..]);
    payload.extend_from_slice(body);

    container_box(kind, &payload)
}

/// One container box, carrying boxes.
fn container_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(body.len().saturating_add(8))
        .expect("a fixture box fits in a file")
        .to_be_bytes()
        .to_vec();

    out.extend_from_slice(kind);
    out.extend_from_slice(body);

    out
}

/// An initialization segment for one track.
fn initialization(handler: &[u8; 4], timescale: u32) -> Vec<u8> {
    let (media_header, entry) = if handler == b"vide" {
        (
            full_box(b"vmhd", 0, 1, &[0; 8]),
            container_box(b"avc1", &[0; 78]),
        )
    } else {
        (
            full_box(b"smhd", 0, 0, &[0; 4]),
            container_box(b"mp4a", &[0; 28]),
        )
    };

    let mut stsd = vec![0, 0, 0, 0]; // version and flags
    stsd.extend_from_slice(&1_u32.to_be_bytes()); // one sample entry
    stsd.extend_from_slice(&entry);

    let mut stbl = container_box(b"stsd", &stsd);
    stbl.extend_from_slice(&full_box(b"stts", 0, 0, &[]));
    stbl.extend_from_slice(&full_box(b"stsc", 0, 0, &[]));

    let mut minf = media_header;
    minf.extend_from_slice(&container_box(
        b"dinf",
        &full_box(b"dref", 0, 0, &[0, 0, 0, 1, 0, 0, 0, 1]),
    ));
    minf.extend_from_slice(&container_box(b"stbl", &stbl));

    // The fixed fields of a track header and a media header, whose lengths are
    // what this fixture has to get right: the version and flags the boxes add
    // in front of them are not part of these.
    let mut tkhd = vec![0_u8; 80];
    tkhd[8..12].copy_from_slice(&1_u32.to_be_bytes());

    let mut mdhd = vec![0_u8; 20];
    mdhd[8..12].copy_from_slice(&timescale.to_be_bytes());

    let mut hdlr = vec![0_u8; 4];
    hdlr.extend_from_slice(handler);

    let mut mdia = full_box(b"mdhd", 0, 0, &mdhd);
    mdia.extend_from_slice(&full_box(b"hdlr", 0, 0, &hdlr));
    mdia.extend_from_slice(&container_box(b"minf", &minf));

    let mut trak = full_box(b"tkhd", 0, 7, &tkhd);
    trak.extend_from_slice(&container_box(b"mdia", &mdia));

    let mut file = container_box(b"ftyp", b"iso5\x00\x00\x02\x00iso5iso6mp41");
    file.extend_from_slice(&container_box(b"moov", &container_box(b"trak", &trak)));

    file
}

/// One media segment: a description of its samples, then their bytes.
fn segment(decode_time: u64, samples: &[(u8, u32)], sample_duration: u32) -> Vec<u8> {
    let describe = |data_offset: i32| {
        let mut traf = full_box(b"tfhd", 0, 0x020000, &1_u32.to_be_bytes());
        traf.extend_from_slice(&full_box(b"tfdt", 1, 0, &decode_time.to_be_bytes()));

        let mut run = u32::try_from(samples.len())
            .expect("a fixture segment holds few samples")
            .to_be_bytes()
            .to_vec();

        run.extend_from_slice(&data_offset.to_be_bytes());

        for (_, size) in samples {
            run.extend_from_slice(&sample_duration.to_be_bytes());
            run.extend_from_slice(&size.to_be_bytes());
            run.extend_from_slice(&0x0200_0000_u32.to_be_bytes()); // stands on its own
            run.extend_from_slice(&0_i32.to_be_bytes()); // plays in order
        }

        traf.extend_from_slice(&full_box(
            b"trun",
            1,
            0x0001 | 0x0100 | 0x0200 | 0x0400 | 0x0800,
            &run,
        ));

        container_box(b"moof", &container_box(b"traf", &traf))
    };

    // The data offset is where the samples begin, which is only known once the
    // description is complete.
    let mut moof = describe(0);
    let offset = i32::try_from(moof.len().saturating_add(8)).expect("a small fragment");

    moof = describe(offset);

    let mut data = Vec::new();

    for (byte, size) in samples {
        data.extend(std::iter::repeat_n(*byte, *size as usize));
    }

    moof.extend_from_slice(&container_box(b"mdat", &data));

    moof
}

/// Where a pattern first appears in a file.
fn position_of(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[tokio::test]
async fn a_presentation_that_keeps_its_sound_apart_is_fetched_and_muxed() {
    let video_init = initialization(b"vide", 30_000);
    let video_first = segment(0, &[(0xa1, 4)], 1000);
    let video_second = segment(1000, &[(0xa2, 4)], 1000);
    let audio_init = initialization(b"soun", 44_100);
    let audio_only = segment(0, &[(0xb1, 8)], 1024);

    let fetched = [
        &video_init,
        &video_first,
        &video_second,
        &audio_init,
        &audio_only,
    ];
    let measured: u64 = fetched
        .iter()
        .map(|body| u64::try_from(body.len()).expect("a fixture length"))
        .sum();

    let entry = serve(
        &[
            (
                "/master.m3u8",
                "application/vnd.apple.mpegurl",
                SPLIT_MASTER.as_bytes(),
            ),
            (
                "/video.m3u8",
                "application/vnd.apple.mpegurl",
                SPLIT_VIDEO.as_bytes(),
            ),
            (
                "/audio.m3u8",
                "application/vnd.apple.mpegurl",
                SPLIT_AUDIO.as_bytes(),
            ),
            ("/v-init.mp4", "video/mp4", &video_init),
            ("/v0.m4s", "video/mp4", &video_first),
            ("/v1.m4s", "video/mp4", &video_second),
            ("/a-init.mp4", "audio/mp4", &audio_init),
            ("/a0.m4s", "audio/mp4", &audio_only),
        ],
        "/master.m3u8",
    )
    .await;

    let directory = tempfile::tempdir().expect("temp directory must exist");
    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));

    let presentation = executor
        .inspect(&RequestContext::new(entry.clone(), Vec::new()))
        .await
        .expect("inspection must succeed");
    let track = &presentation.tracks()[0];

    assert_eq!(track.container(), None);
    assert_eq!(track.filename(), None);

    let prepared = executor
        .prepare(input_unnamed(entry, directory.path()))
        .await
        .expect("prepare must succeed");

    // Both tracks are measured, and the download is named after the
    // presentation rather than after either track's playlist.
    assert_eq!(prepared.total_bytes(), Some(measured));
    assert_eq!(
        prepared.planned_destination().final_path(),
        directory.path().join("master.mp4").as_path()
    );

    let outcome = prepared.transfer().await.expect("transfer must succeed");

    let TransferOutcome::Finished(staged) = outcome else {
        panic!("the transfer must finish");
    };

    // Progress counts what was fetched; the published file is smaller because
    // initialization data and container headers are not media.
    assert_eq!(staged.written_bytes(), measured);

    let output = staged.finalize().await.expect("finalize must succeed");
    let bytes = std::fs::read(output.destination.final_path()).expect("output must be readable");

    assert_eq!(output.written_bytes, measured);
    assert!(
        u64::try_from(bytes.len()).expect("a length") < measured,
        "the muxed file must be smaller than the fetched segments"
    );

    // The picture is followed by the sound, exactly as the segments carried
    // them, and neither initialization segment is in the file.
    let video = position_of(&bytes, &[0xa1; 4]).expect("the first video sample is published");
    let later = position_of(&bytes, &[0xa2; 4]).expect("the second video sample is published");
    let audio = position_of(&bytes, &[0xb1; 8]).expect("the audio sample is published");

    assert!(video < later && later < audio);

    let leftovers: Vec<String> = std::fs::read_dir(directory.path())
        .expect("directory must be readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains(".part"))
        .collect();

    assert!(
        leftovers.is_empty(),
        "unexpected staging files: {leftovers:?}"
    );
}

// The fixtures below are a transport-stream presentation, which is what HLS
// serves when a playlist carries no `EXT-X-MAP`. The muxer has to demux both
// tracks before it can write a file, so these fixtures build real packets and
// real elementary streams rather than placeholders.

/// A master playlist whose video and audio are both transport streams.
const TS_SPLIT_MASTER: &str = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=1280x720,AUDIO=\"aac\"\n\
video.m3u8\n";

const TS_SPLIT_VIDEO: &str = "#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXTINF:4,\n\
v0.ts\n\
#EXTINF:4,\n\
v1.ts\n\
#EXT-X-ENDLIST\n";

const TS_SPLIT_AUDIO: &str = "#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXTINF:4,\n\
a0.ts\n\
#EXT-X-ENDLIST\n";

/// One transport-stream packet, with an adaptation field carrying the stuffing
/// so the payload is exactly what the fixture wrote.
fn ts_packet(pid: u16, start: bool, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= 184, "a fixture packet fits in one packet");

    let mut packet = Vec::with_capacity(188);
    packet.push(0x47);
    packet.push((((pid >> 8) as u8) & 0x1F) | if start { 0x40 } else { 0 });
    packet.push(pid as u8);

    let stuffing = 184 - payload.len();

    if stuffing == 0 {
        packet.push(0x10);
    } else {
        packet.push(0x30); // adaptation field, then payload
        packet.push((stuffing - 1) as u8);

        if stuffing > 1 {
            packet.push(0x00); // adaptation flags
            packet.extend(std::iter::repeat_n(0xFF, stuffing - 2));
        }
    }

    packet.extend_from_slice(payload);
    assert_eq!(packet.len(), 188);

    packet
}

/// A program association table naming one program.
fn ts_pat(pmt_pid: u16) -> Vec<u8> {
    let mut body = vec![0x00, 0x01, 0xC1, 0x00, 0x00];
    body.extend_from_slice(&1_u16.to_be_bytes());
    body.push(0xE0 | (pmt_pid >> 8) as u8);
    body.push(pmt_pid as u8);

    ts_section(0x00, &body)
}

/// A program map table naming one video and one audio stream.
fn ts_pmt(video_pid: u16, audio_pid: u16) -> Vec<u8> {
    let mut body = vec![0x00, 0x01, 0xC1, 0x00, 0x00];
    body.extend_from_slice(&0x1FFF_u16.to_be_bytes());
    body.extend_from_slice(&0xF000_u16.to_be_bytes());
    body.push(0x1B);
    body.push(0xE0 | (video_pid >> 8) as u8);
    body.push(video_pid as u8);
    body.extend_from_slice(&0xF000_u16.to_be_bytes());
    body.push(0x0F);
    body.push(0xE0 | (audio_pid >> 8) as u8);
    body.push(audio_pid as u8);
    body.extend_from_slice(&0xF000_u16.to_be_bytes());

    ts_section(0x02, &body)
}

/// Wraps a table body in its section header and a placeholder check value.
fn ts_section(table: u8, body: &[u8]) -> Vec<u8> {
    let length = body.len() + 4;
    let mut section = vec![table, 0xB0 | ((length >> 8) as u8 & 0x0F), length as u8];
    section.extend_from_slice(body);
    section.extend_from_slice(&[0; 4]);

    let mut payload = vec![0x00]; // pointer field
    payload.extend_from_slice(&section);

    payload
}

/// A PES packet carrying an elementary stream and its presentation time.
fn ts_pes(payload: &[u8], pts: u64) -> Vec<u8> {
    let mut header = vec![0x00, 0x00, 0x01, 0xE0];
    header.extend_from_slice(&0_u16.to_be_bytes());
    header.push(0x80);
    header.push(0x80); // a presentation time only
    header.push(5);
    header.push(0x21 | (((pts >> 30) as u8 & 0x07) << 1));
    header.push((pts >> 22) as u8);
    header.push(0x01 | (((pts >> 15) as u8 & 0x7F) << 1));
    header.push((pts >> 7) as u8);
    header.push(0x01 | ((pts as u8 & 0x7F) << 1));
    header.extend_from_slice(payload);

    header
}

/// A NAL unit with its Annex B start code.
fn ts_nal(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut unit = vec![0x00, 0x00, 0x00, 0x01, kind];
    unit.extend_from_slice(body);

    unit
}

/// A sequence parameter set for a 1280x720 baseline picture.
fn ts_sps() -> Vec<u8> {
    let mut bits = TsBits::default();
    bits.unsigned(0); // identifier
    bits.unsigned(0); // log2_max_frame_num_minus4
    bits.unsigned(0); // pic_order_cnt_type
    bits.unsigned(0); // log2_max_pic_order_cnt_lsb_minus4
    bits.unsigned(1); // max_num_ref_frames
    bits.bit(0); // gaps
    bits.unsigned(79); // 1280 pixels
    bits.unsigned(44); // 720 pixels
    bits.bit(1); // frame_mbs_only_flag
    bits.bit(0); // direct_8x8_inference_flag
    bits.bit(0); // no cropping
    bits.bit(0); // no VUI

    let mut sps = vec![0x67, 0x42, 0xC0, 0x1E];
    sps.extend_from_slice(&bits.finish());

    sps
}

/// Writes the Exp-Golomb codes a parameter set is made of.
#[derive(Default)]
struct TsBits {
    bits: Vec<u8>,
}

impl TsBits {
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

/// One ADTS frame holding `size` bytes of AAC.
fn ts_adts_frame(byte: u8, size: usize) -> Vec<u8> {
    let length = size + 7;

    let mut frame = vec![
        0xFF,
        0xF1,
        0x50, // AAC-LC at 44.1 kHz
        0x80 | ((length >> 11) as u8 & 0x03),
        (length >> 3) as u8,
        ((length as u8 & 0x07) << 5) | 0x1F,
        0xFC,
    ];

    frame.extend(std::iter::repeat_n(byte, size));

    frame
}

/// One complete video segment: the program tables, then one access unit.
fn ts_video_segment(access_unit: &[u8], pts: u64) -> Vec<u8> {
    let mut segment = Vec::new();
    segment.extend(ts_packet(0, true, &ts_pat(0x1000)));
    segment.extend(ts_packet(0x1000, true, &ts_pmt(0x0100, 0x0101)));
    segment.extend(ts_packet(0x0100, true, &ts_pes(access_unit, pts)));

    segment
}

/// One complete audio segment: the program tables, then its AAC frames.
fn ts_audio_segment(frames: &[u8]) -> Vec<u8> {
    let mut segment = Vec::new();
    segment.extend(ts_packet(0, true, &ts_pat(0x1000)));
    segment.extend(ts_packet(0x1000, true, &ts_pmt(0x0100, 0x0101)));
    segment.extend(ts_packet(0x0101, true, &ts_pes(frames, 0)));

    segment
}

#[tokio::test]
async fn a_transport_stream_presentation_is_demuxed_and_muxed() {
    let first = ts_video_segment(
        &[
            ts_nal(0x09, &[0xF0]),
            ts_nal(0x67, &ts_sps()[1..]),
            ts_nal(0x68, &[0xCE, 0x3C, 0x80]),
            ts_nal(0x65, &[0x88, 0x84, 0x00, 0x33]),
        ]
        .concat(),
        0,
    );

    let second = ts_video_segment(
        &[ts_nal(0x09, &[0xF0]), ts_nal(0x41, &[0x9A, 0x02, 0x03])].concat(),
        3_000,
    );

    let audio = ts_audio_segment(&[ts_adts_frame(0x11, 4), ts_adts_frame(0x22, 4)].concat());

    let fetched = [&first, &second, &audio];
    let measured: u64 = fetched
        .iter()
        .map(|body| u64::try_from(body.len()).expect("a fixture length"))
        .sum();

    let entry = serve(
        &[
            (
                "/master.m3u8",
                "application/vnd.apple.mpegurl",
                TS_SPLIT_MASTER.as_bytes(),
            ),
            (
                "/video.m3u8",
                "application/vnd.apple.mpegurl",
                TS_SPLIT_VIDEO.as_bytes(),
            ),
            (
                "/audio.m3u8",
                "application/vnd.apple.mpegurl",
                TS_SPLIT_AUDIO.as_bytes(),
            ),
            ("/v0.ts", "video/mp2t", &first),
            ("/v1.ts", "video/mp2t", &second),
            ("/a0.ts", "audio/mp2t", &audio),
        ],
        "/master.m3u8",
    )
    .await;

    let directory = tempfile::tempdir().expect("temp directory must exist");
    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));

    let prepared = executor
        .prepare(input_unnamed(entry, directory.path()))
        .await
        .expect("prepare must succeed");

    assert_eq!(prepared.total_bytes(), Some(measured));
    assert_eq!(
        prepared.planned_destination().final_path(),
        directory.path().join("master.mp4").as_path()
    );

    let outcome = prepared.transfer().await.expect("transfer must succeed");

    let TransferOutcome::Finished(staged) = outcome else {
        panic!("the transfer must finish");
    };

    let output = staged.finalize().await.expect("finalize must succeed");
    let bytes = std::fs::read(output.destination.final_path()).expect("output must be readable");

    assert_eq!(&bytes[4..8], b"ftyp");
    assert_eq!(
        bytes.windows(4).filter(|window| *window == b"trak").count(),
        2
    );

    // The sample entries say what the transport streams carried: an H.264
    // picture 1280 pixels wide and 720 high, and an AAC sound.
    let video = position_of(&bytes, b"avc1").expect("the video sample entry is written");
    let audio = position_of(&bytes, b"mp4a").expect("the audio sample entry is written");

    assert_eq!(
        u16::from_be_bytes([bytes[video + 28], bytes[video + 29]]),
        1280
    );
    assert_eq!(
        u16::from_be_bytes([bytes[video + 30], bytes[video + 31]]),
        720
    );
    assert!(video < audio);

    let leftovers: Vec<String> = std::fs::read_dir(directory.path())
        .expect("directory must be readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains(".part") || name.contains(".elementary"))
        .collect();

    assert!(
        leftovers.is_empty(),
        "unexpected staging files: {leftovers:?}"
    );
}
