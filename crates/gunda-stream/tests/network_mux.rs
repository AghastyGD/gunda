//! A real presentation, fetched and muxed, for evidence rather than for CI.
//!
//! These tests use the network and write into `target/mux-probe`, so they only
//! run when they are asked for:
//!
//! ```text
//! cargo test -p gunda-stream --test network_mux -- --ignored --nocapture
//! ```
//!
//! What they establish is that a presentation an encoder actually produced, with
//! its sound in a track of its own, is fetched, combined and published as one
//! file. Whether that file plays, and what an independent reader makes of it, is
//! checked outside this suite against the published file itself.

use gunda_core::application::{
    DownloadExecutor, ExecutionInput, PreparedTransfer, StagedTransfer, TransferOutcome,
};
use gunda_core::download::{
    DownloadDestination, DownloadId, DownloadProgress, FileConflictPolicy, RequestContext,
};
use gunda_http::HttpClient;
use gunda_stream::StreamExecutor;
use url::Url;

/// A presentation that keeps its picture and its sound in separate tracks.
const PRESENTATION: &str =
    "https://storage.googleapis.com/shaka-demo-assets/angel-one-hls/hls.m3u8";

/// Builds an execution with no filename chosen, as a capture would.
fn input(url: Url, directory: &std::path::Path) -> ExecutionInput {
    ExecutionInput {
        id: DownloadId::new(1).expect("ID must be valid"),
        request: RequestContext::new(url, Vec::new()),
        destination: DownloadDestination::new(
            directory.to_path_buf(),
            None,
            FileConflictPolicy::Fail,
        ),
        progress: DownloadProgress::default(),
        resource: None,
        resolved_destination: None,
    }
}

#[tokio::test]
#[ignore = "uses the network"]
async fn a_real_presentation_with_separate_audio_is_muxed_into_one_file() {
    let url = Url::parse(PRESENTATION).expect("URL must parse");

    let directory =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/mux-probe");
    std::fs::create_dir_all(&directory).expect("probe directory must exist");

    let executor = StreamExecutor::new(HttpClient::new().expect("client must build"));

    let presentation = executor
        .inspect(&RequestContext::new(url.clone(), Vec::new()))
        .await
        .expect("inspection must succeed");

    for track in presentation.tracks() {
        println!(
            "track: {:?} [{}] size={:?} duration={:?} file={:?}",
            track.kind(),
            track.label(),
            track.size(),
            track.duration_millis(),
            track.filename()
        );
    }

    let prepared = executor
        .prepare(input(url, &directory))
        .await
        .expect("prepare must succeed");

    println!(
        "planned: {} bytes known, publishing {:?}",
        prepared.total_bytes().unwrap_or(0),
        prepared.planned_destination().final_path()
    );

    let outcome = prepared.transfer().await.expect("transfer must succeed");

    let TransferOutcome::Finished(staged) = outcome else {
        panic!("the transfer must finish");
    };

    let output = staged.finalize().await.expect("finalize must succeed");
    let published = output.destination.final_path();
    let length = std::fs::metadata(published)
        .expect("the published file must exist")
        .len();

    println!(
        "published: {} bytes at {:?} ({} bytes reported)",
        length, published, output.written_bytes
    );

    assert!(output.written_bytes >= length);
    assert!(length > 1024);
}

/// A presentation an encoder produced as transport streams, with the sound in
/// a rendition of its own.
const TRANSPORT_PRESENTATION: &str = "https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_ts/master.m3u8";

/// How many media segments of each track the transport-stream probe fetches.
///
/// The point is to demux and mux bytes a real encoder produced, not to download
/// ten minutes of video; three segments are enough for sample tables, timing
/// and codec configuration.
const TRANSPORT_SEGMENTS: usize = 3;

/// Fetches one resource into memory.
async fn fetch(client: &HttpClient, url: &Url) -> Vec<u8> {
    let mut body = client
        .open(&RequestContext::new(url.clone(), Vec::new()))
        .await
        .expect("the resource must open");
    let mut bytes = Vec::new();

    while let Some(chunk) = body.next_chunk().await.expect("the body must arrive") {
        bytes.extend_from_slice(&chunk);
    }

    bytes
}

/// Fetches a playlist and parses it.
async fn fetch_playlist(client: &HttpClient, url: &Url) -> gunda_stream::hls::Playlist {
    let bytes = fetch(client, url).await;
    let text = std::str::from_utf8(&bytes).expect("a playlist is text");

    gunda_stream::hls::parse(text).expect("a playlist must parse")
}

/// The first media segments of a playlist, resolved against it.
fn first_segments(playlist: &gunda_stream::hls::MediaPlaylist, base: &Url) -> Vec<Url> {
    playlist
        .segments()
        .iter()
        .take(TRANSPORT_SEGMENTS)
        .map(|segment| {
            base.join(segment.resource().uri())
                .expect("a segment reference must resolve")
        })
        .collect()
}

#[tokio::test]
#[ignore = "uses the network"]
async fn a_real_transport_stream_pair_is_demuxed_and_muxed() {
    let master_url = Url::parse(TRANSPORT_PRESENTATION).expect("URL must parse");
    let client = HttpClient::new().expect("client must build");

    let gunda_stream::hls::Playlist::Master(master) = fetch_playlist(&client, &master_url).await
    else {
        panic!("the entry point must be a master playlist");
    };

    // The master offers the same picture with stereo AAC and with surround
    // Dolby. Only the AAC rendition can be read out of a transport stream, so
    // the probe takes the best variant that declares it, which is also what the
    // planner now selects.
    let variant = master
        .variants()
        .iter()
        .filter(|variant| {
            variant.codecs().is_some_and(|codecs| {
                codecs
                    .split(',')
                    .any(|codec| codec.trim().starts_with("mp4a"))
            })
        })
        .max_by_key(|variant| variant.bandwidth().unwrap_or(0))
        .expect("a variant with AAC audio must exist");

    let rendition = master
        .default_audio_rendition(variant)
        .expect("the chosen variant must name an audio rendition");
    let variant_url = master_url
        .join(variant.uri())
        .expect("the variant URL must resolve");
    let audio_url = master_url
        .join(rendition.uri().expect("a rendition with a playlist"))
        .expect("the audio URL must resolve");

    println!("variant: {variant_url}");
    println!("audio:   {audio_url}");

    let gunda_stream::hls::Playlist::Media(video) = fetch_playlist(&client, &variant_url).await
    else {
        panic!("the variant must be a media playlist");
    };
    let gunda_stream::hls::Playlist::Media(sound) = fetch_playlist(&client, &audio_url).await
    else {
        panic!("the rendition must be a media playlist");
    };

    let video_segments = first_segments(&video, &variant_url);
    let audio_segments = first_segments(&sound, &audio_url);

    assert!(
        !video_segments.is_empty() && !audio_segments.is_empty(),
        "the presentation must have media"
    );

    let mut video_bytes = Vec::new();
    let mut audio_bytes = Vec::new();

    for url in &video_segments {
        let bytes = fetch(&client, url).await;
        println!("video segment: {} bytes from {url}", bytes.len());
        video_bytes.extend_from_slice(&bytes);
    }

    for url in &audio_segments {
        let bytes = fetch(&client, url).await;
        println!("audio segment: {} bytes from {url}", bytes.len());
        audio_bytes.extend_from_slice(&bytes);
    }

    let directory =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/mux-probe");
    std::fs::create_dir_all(&directory).expect("probe directory must exist");

    let video_path = directory.join("video-segments.ts");
    let audio_path = directory.join("audio-segments.aac");
    let output_path = directory.join("ts.mp4");

    std::fs::write(&video_path, &video_bytes).expect("the video track must be written");
    std::fs::write(&audio_path, &audio_bytes).expect("the audio track must be written");

    let written = gunda_stream::mux_mp4(&video_path, &audio_path, &output_path)
        .await
        .expect("the tracks must be demuxed and muxed");

    let bytes = std::fs::read(&output_path).expect("the output must be readable");

    println!(
        "published: {} bytes ({} reported) at {output_path:?}",
        bytes.len(),
        written
    );

    assert_eq!(
        u64::try_from(bytes.len()).expect("a length"),
        written,
        "the reported size must be the file's size"
    );
    assert_eq!(&bytes[4..8], b"ftyp");
    assert_eq!(
        bytes.windows(4).filter(|window| *window == b"trak").count(),
        2,
        "both tracks must be in the file"
    );
    assert!(
        bytes.windows(4).any(|window| window == b"avc1"),
        "the video sample entry must be written"
    );
    assert!(
        bytes.windows(4).any(|window| window == b"mp4a"),
        "the audio sample entry must be written"
    );
}
