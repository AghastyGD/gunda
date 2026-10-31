//! The capture channel as the desktop sees it.
//!
//! The channel itself is tested in `gunda-capture`. What matters here is the
//! wiring: a resource the browser reports reaches the snapshot the interface
//! renders, and the user turning it into a download produces a job attributed to
//! the page it came from.

use super::*;

use gunda_capture::{Catalog, Report};
use gunda_stream::StreamResumeStore;

/// A master playlist offering one quality, so the choice is unambiguous.
const TRACK_MASTER: &str = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\n\
low.m3u8\n";

/// A variant that needs an initialization segment, so it is fragmented MP4.
const TRACK_VARIANT: &str = "#EXTM3U\n\
#EXT-X-MAP:URI=\"init.mp4\"\n\
#EXTINF:4,\n\
s0.m4s\n\
#EXT-X-ENDLIST\n";

const QUALITY_MASTER: &str = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\n\
low.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\n\
high.m3u8\n";

const LOW_VARIANT: &str = "#EXTM3U\n\
#EXT-X-MAP:URI=\"low-init.mp4\"\n\
#EXTINF:4,\n\
low0.m4s\n\
#EXT-X-ENDLIST\n";

const HIGH_VARIANT: &str = "#EXTM3U\n\
#EXT-X-MAP:URI=\"high-init.mp4\"\n\
#EXTINF:4,\n\
high0.m4s\n\
#EXT-X-ENDLIST\n";

const SPLIT_PLAYLIST: &str = "#EXTM3U\n\
#EXTINF:1,\ns0.ts\n\
#EXTINF:1,\ns1.ts\n\
#EXTINF:1,\ns2.ts\n\
#EXT-X-ENDLIST\n";

#[test]
fn picks_the_segmented_engine_for_manifests() {
    let from_url = |value: &str| engine_for(&Url::parse(value).unwrap(), None);
    let from_resource = |value: &str, kind| engine_for(&Url::parse(value).unwrap(), Some(kind));

    assert_eq!(
        from_url("https://cdn.example.com/master.m3u8"),
        JobEngine::Stream
    );
    assert_eq!(
        from_url("https://cdn.example.com/stream.mpd?token=x"),
        JobEngine::Stream
    );
    assert_eq!(
        from_url("https://cdn.example.com/MOVIE.MP4"),
        JobEngine::Http
    );
    // What capture recorded decides, so a manifest whose URL does not name its
    // format still reaches the engine that can read it.
    assert_eq!(
        from_resource("https://cdn.example.com/play", ResourceKind::Hls),
        JobEngine::Stream
    );
    assert_eq!(
        from_resource("https://cdn.example.com/play", ResourceKind::Dash),
        JobEngine::Stream
    );
    assert_eq!(
        from_resource("https://cdn.example.com/movie.mp4", ResourceKind::File),
        JobEngine::Http
    );
}

#[test]
fn refuses_a_manifest_that_does_not_name_its_format() {
    // A playlist recognised from its content type offers no extension to point
    // the segmented engine at. The plain engine would quietly save the playlist
    // itself as a file, so the capture is refused instead.
    let playlist = candidate(
        "https://cdn.example.com/play",
        Some("application/vnd.apple.mpegurl"),
    );

    assert_eq!(playlist.kind(), ResourceKind::Hls);
    assert!(capture_engine(&playlist).is_err());

    let video = candidate("https://cdn.example.com/movie.mp4", Some("video/mp4"));

    assert_eq!(capture_engine(&video).unwrap(), JobEngine::Http);

    let manifest = candidate("https://cdn.example.com/master.m3u8", None);

    assert_eq!(capture_engine(&manifest).unwrap(), JobEngine::Stream);
}

/// Builds a candidate exactly as the catalog does.
fn candidate(url: &str, content_type: Option<&str>) -> Candidate {
    let mut catalog = Catalog::new(8);
    let recorded = catalog.record(
        Report::new(url, content_type, "https://example.com/watch", None)
            .expect("the report is valid"),
    );

    catalog
        .get(recorded.id)
        .expect("the candidate is held")
        .clone()
}

#[tokio::test]
async fn a_reported_resource_reaches_the_interface_and_downloads_as_a_browser_job() {
    let data = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let (media, server) = serve(
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: video/mp4\r\nConnection: close\r\n\r\nhello",
    )
    .await;
    let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
    let listening = wait_for(&mut backend, |snapshot| {
        snapshot.ready && snapshot.capture.is_some()
    })
    .await;

    assert_eq!(listening.capture_problem, None);

    let status = listening.capture.clone().expect("the channel is running");
    let mut extension = Extension::connect(&status.endpoint).await;

    extension.greet(&status.token).await;
    extension.report(media.as_str(), "video/mp4").await;

    let captured = wait_for(&mut backend, |snapshot| !snapshot.captures.is_empty()).await;
    let view = &captured.captures[0];

    assert_eq!(view.url, media.as_str());
    assert_eq!(view.kind, "File");
    assert_eq!(view.content_type.as_deref(), Some("video/mp4"));
    assert_eq!(view.page_title.as_deref(), Some("A movie"));
    assert_eq!(view.seen, 1);

    backend
        .send(Command::Capture {
            id: view.id,
            directory: output.path().into(),
            track: None,
        })
        .unwrap();

    let completed = wait_for(&mut backend, |snapshot| {
        !snapshot.busy
            && snapshot
                .jobs
                .first()
                .is_some_and(|view| view.state == DownloadState::Completed)
    })
    .await;
    let job = &completed.jobs[0];

    assert_eq!(job.origin, "Browser");
    assert_eq!(
        std::fs::read(job.output.as_ref().unwrap()).unwrap(),
        b"hello"
    );

    let repository = SqliteDownloadRepository::open(data.path().join("gunda.sqlite3"))
        .await
        .unwrap();
    let saved = repository
        .find_by_id(job.id)
        .await
        .unwrap()
        .expect("the job is persisted");
    let DownloadOrigin::Browser {
        page_url,
        page_title,
    } = saved.origin()
    else {
        panic!("a capture is attributed to its page");
    };

    assert_eq!(page_url.as_str(), "https://example.com/watch");
    assert_eq!(page_title.as_deref(), Some("A movie"));

    // Dismissing forgets the capture, not the download it produced.
    backend.send(Command::DismissCapture(view.id)).unwrap();

    let dismissed = wait_for(&mut backend, |snapshot| snapshot.captures.is_empty()).await;

    assert_eq!(dismissed.jobs[0].state, DownloadState::Completed);
    server.await.unwrap();
}

#[tokio::test]
async fn a_chosen_quality_downloads_and_is_named_after_the_presentation() {
    let data = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let base = serve_routes(&[
        (
            "/master.m3u8",
            "application/vnd.apple.mpegurl",
            TRACK_MASTER.as_bytes(),
        ),
        (
            "/low.m3u8",
            "application/vnd.apple.mpegurl",
            TRACK_VARIANT.as_bytes(),
        ),
        ("/init.mp4", "video/mp4", b"INIT"),
        ("/s0.m4s", "video/mp4", b"SEG0"),
    ])
    .await;
    let master = base.join("master.m3u8").unwrap();

    let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
    let listening = wait_for(&mut backend, |snapshot| {
        snapshot.ready && snapshot.capture.is_some()
    })
    .await;
    let status = listening.capture.clone().expect("the channel is running");
    let mut extension = Extension::connect(&status.endpoint).await;

    extension.greet(&status.token).await;
    extension
        .report(master.as_str(), "application/vnd.apple.mpegurl")
        .await;

    let captured = wait_for(&mut backend, |snapshot| !snapshot.captures.is_empty()).await;
    let view = &captured.captures[0];

    assert_eq!(view.kind, "HLS");
    assert!(view.segmented);

    // Reading the qualities is a step of its own: nothing is fetched from the
    // presentation until the user asks for them.
    assert!(captured.capture_tracks.is_none());

    backend.send(Command::Inspect(view.id)).unwrap();

    let inspected = wait_for(&mut backend, |snapshot| snapshot.capture_tracks.is_some()).await;
    let tracks = inspected.capture_tracks.as_ref().unwrap();

    assert_eq!(tracks.id, view.id);
    assert_eq!(tracks.problem, None);
    assert_eq!(tracks.tracks.len(), 1);

    let track = &tracks.tracks[0];

    assert_eq!(track.label, "640x360 (1.0 Mbps)");
    assert_eq!(track.details, "video | size unknown");
    assert_eq!(track.filename, None);
    assert_eq!(track.url, master);
    assert!(track.selection.is_some());

    backend
        .send(Command::Capture {
            id: view.id,
            directory: output.path().into(),
            track: Some(TrackChoice {
                url: track.url.clone(),
                filename: track.filename.clone(),
                selection: track.selection.clone(),
            }),
        })
        .unwrap();

    let completed = wait_for(&mut backend, |snapshot| {
        !snapshot.busy
            && snapshot
                .jobs
                .first()
                .is_some_and(|view| view.state == DownloadState::Completed)
    })
    .await;
    let job = &completed.jobs[0];
    let written = job.output.as_ref().expect("the download is published");

    // The output is the chosen track, named after the presentation rather than
    // after the variant's own playlist.
    assert_eq!(written.file_name().unwrap(), "master.mp4");
    assert_eq!(std::fs::read(written).unwrap(), b"INITSEG0");
    assert!(completed.capture_tracks.is_none());
}

#[tokio::test]
async fn choosing_a_lower_quality_downloads_that_variant() {
    let data = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let base = serve_routes(&[
        (
            "/master.m3u8",
            "application/vnd.apple.mpegurl",
            QUALITY_MASTER.as_bytes(),
        ),
        (
            "/low.m3u8",
            "application/vnd.apple.mpegurl",
            LOW_VARIANT.as_bytes(),
        ),
        (
            "/high.m3u8",
            "application/vnd.apple.mpegurl",
            HIGH_VARIANT.as_bytes(),
        ),
        ("/low-init.mp4", "video/mp4", b"LOWINIT"),
        ("/low0.m4s", "video/mp4", b"LOWSEG"),
        ("/high-init.mp4", "video/mp4", b"HIGHINIT"),
        ("/high0.m4s", "video/mp4", b"HIGHSEG"),
    ])
    .await;
    let master = base.join("master.m3u8").unwrap();

    let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
    wait_for(&mut backend, |snapshot| {
        snapshot.ready && snapshot.capture.is_some()
    })
    .await;
    let status = backend.snapshots.borrow().capture.clone().unwrap();
    let mut extension = Extension::connect(&status.endpoint).await;

    extension.greet(&status.token).await;
    extension
        .report(master.as_str(), "application/vnd.apple.mpegurl")
        .await;

    let captured = wait_for(&mut backend, |snapshot| !snapshot.captures.is_empty()).await;
    let view = &captured.captures[0];
    backend.send(Command::Inspect(view.id)).unwrap();

    let inspected = wait_for(&mut backend, |snapshot| snapshot.capture_tracks.is_some()).await;
    let tracks = &inspected.capture_tracks.as_ref().unwrap().tracks;

    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].label, "1920x1080 (5.0 Mbps)");
    assert_eq!(tracks[1].label, "640x360 (1.0 Mbps)");

    let chosen = tracks[1].clone();
    backend
        .send(Command::Capture {
            id: view.id,
            directory: output.path().into(),
            track: Some(TrackChoice {
                url: chosen.url,
                filename: chosen.filename,
                selection: chosen.selection,
            }),
        })
        .unwrap();

    let completed = wait_for(&mut backend, |snapshot| {
        !snapshot.busy
            && snapshot
                .jobs
                .first()
                .is_some_and(|view| view.state == DownloadState::Completed)
    })
    .await;
    let job = &completed.jobs[0];
    let written = job.output.as_ref().expect("the download is published");

    assert_eq!(std::fs::read(written).unwrap(), b"LOWINITLOWSEG");

    let repository = SqliteDownloadRepository::open(data.path().join("gunda.sqlite3"))
        .await
        .unwrap();
    let saved = repository.find_by_id(job.id).await.unwrap().unwrap();

    assert!(saved.request().selection().is_some());
}

#[tokio::test]
async fn a_segmented_download_pauses_and_resumes_from_its_checkpoint() {
    let data = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let base = serve_delayed_routes(
        &[
            (
                "/media.m3u8",
                "application/vnd.apple.mpegurl",
                SPLIT_PLAYLIST.as_bytes(),
            ),
            ("/s0.ts", "video/mp2t", b"SEG0"),
            ("/s1.ts", "video/mp2t", b"SEG1"),
            ("/s2.ts", "video/mp2t", b"SEG2"),
        ],
        "/s1.ts",
    )
    .await;
    let entry = base.join("media.m3u8").unwrap();
    let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();

    wait_for(&mut backend, |snapshot| snapshot.ready).await;
    backend
        .send(Command::Start {
            url: entry,
            directory: output.path().into(),
        })
        .unwrap();

    let running = wait_for(&mut backend, |snapshot| {
        snapshot.jobs.first().is_some_and(|view| view.written >= 4)
            && snapshot
                .jobs
                .first()
                .is_some_and(|view| view.pause_supported)
            && !snapshot.active.is_empty()
    })
    .await;
    let id = *running.active.iter().next().expect("the stream is active");

    assert!(backend.pause(id), "the segmented transfer must be pausable");

    let paused = wait_for(&mut backend, |snapshot| {
        snapshot
            .jobs
            .first()
            .is_some_and(|view| view.state == DownloadState::Paused && view.resume_available)
    })
    .await;
    let paused_job = &paused.jobs[0];
    assert_eq!(paused_job.written, 4);

    let repository = SqliteDownloadRepository::open(data.path().join("gunda.sqlite3"))
        .await
        .unwrap();
    let state = repository
        .stream_resume_store()
        .find(id)
        .await
        .unwrap()
        .expect("the checkpoint survives the pause");
    assert_eq!(state.tracks()[0].next_resource(), 1);
    assert_eq!(state.tracks()[0].durable_bytes(), 4);
    repository.close().await;

    backend.send(Command::Resume(id)).unwrap();

    let completed = wait_for(&mut backend, |snapshot| {
        !snapshot.busy
            && snapshot
                .jobs
                .first()
                .is_some_and(|view| view.state == DownloadState::Completed)
    })
    .await;
    let written = completed.jobs[0]
        .output
        .as_ref()
        .expect("the resumed download is published");

    assert_eq!(std::fs::read(written).unwrap(), b"SEG0SEG1SEG2");
}

/// Serves several paths, as an origin carrying a presentation does.
///
/// The backend reads a manifest, then each variant's playlist, then the
/// segments, so the fixture has to answer more than once.
async fn serve_routes(routes: &[(&str, &str, &[u8])]) -> Url {
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
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_request(&mut socket).await;
            let head = String::from_utf8_lossy(&request);
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();

            let response = match routes.iter().find(|(route, _, _)| *route == path) {
                Some((_, content_type, body)) => {
                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();

                    response.extend_from_slice(body);
                    response
                }
                None => b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\
                          connection: close\r\n\r\n"
                    .to_vec(),
            };

            let _ = socket.write_all(&response).await;
        }
    });

    Url::parse(&format!("http://{address}/")).unwrap()
}

/// Serves routes and holds one response body back long enough to pause there.
async fn serve_delayed_routes(routes: &[(&str, &str, &[u8])], delayed: &str) -> Url {
    let routes: Vec<(String, String, Vec<u8>, bool)> = routes
        .iter()
        .map(|(path, content_type, body)| {
            (
                (*path).to_owned(),
                (*content_type).to_owned(),
                body.to_vec(),
                *path == delayed,
            )
        })
        .collect();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_request(&mut socket).await;
            let head = String::from_utf8_lossy(&request);
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();

            let response = match routes.iter().find(|(route, _, _, _)| *route == path) {
                Some((_, content_type, body, delayed)) => {
                    if *delayed {
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    }

                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();

                    response.extend_from_slice(body);
                    response
                }
                None => b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\
                          connection: close\r\n\r\n"
                    .to_vec(),
            };

            let _ = socket.write_all(&response).await;
        }
    });

    Url::parse(&format!("http://{address}/")).unwrap()
}

/// The browser half of the channel, as far as one report needs it.
///
/// `gunda-capture`'s own client is private to it, so this is the other half
/// written again from the protocol: a browser sends masked frames and has to
/// complete the upgrade first.
struct Extension {
    socket: tokio::net::TcpStream,
}

impl Extension {
    /// Opens the WebSocket from an extension's origin.
    async fn connect(endpoint: &str) -> Self {
        let url = Url::parse(endpoint).expect("the endpoint is a URL");
        let address = format!(
            "{}:{}",
            url.host_str().expect("the endpoint names a host"),
            url.port().expect("the endpoint names a port")
        );
        let mut socket = tokio::net::TcpStream::connect(&address).await.unwrap();
        let request = format!(
            "GET {} HTTP/1.1\r\n\
             Host: {address}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Origin: chrome-extension://gunda-test-extension\r\n\
             \r\n",
            url.path()
        );

        socket.write_all(request.as_bytes()).await.unwrap();

        let head = read_head(&mut socket).await;

        assert!(
            head.starts_with("HTTP/1.1 101"),
            "the upgrade was refused: {head}"
        );

        Self { socket }
    }

    /// Greets the channel and checks it came up.
    async fn greet(&mut self, token: &str) {
        self.send(&format!(
            r#"{{"kind":"hello","version":3,"token":"{token}","agent":"gunda-test"}}"#
        ))
        .await;

        assert_eq!(
            self.receive().await,
            r#"{"kind":"ready","version":3,"features":["browser_download"]}"#
        );
    }

    /// Reports one media resource and checks the identifier it was given.
    async fn report(&mut self, url: &str, content_type: &str) {
        self.send(&format!(
            r#"{{"kind":"candidate","url":"{url}","content_type":"{content_type}","page_url":"https://example.com/watch","page_title":"A movie"}}"#
        ))
        .await;

        assert_eq!(self.receive().await, r#"{"kind":"accepted","id":0}"#);
    }

    /// Sends one masked text frame, as a browser must.
    async fn send(&mut self, message: &str) {
        let payload = message.as_bytes();
        let mask = [0x51_u8, 0x62, 0x73, 0x84];
        let mut frame = vec![0x81];

        if payload.len() < 126 {
            frame.push(0x80 | u8::try_from(payload.len()).expect("the length fits"));
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(
                &u16::try_from(payload.len())
                    .expect("the message is short")
                    .to_be_bytes(),
            );
        }

        frame.extend_from_slice(&mask);

        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }

        self.socket.write_all(&frame).await.unwrap();
    }

    /// Reads one unmasked text frame from the channel.
    async fn receive(&mut self) -> String {
        let mut header = [0_u8; 2];

        self.socket.read_exact(&mut header).await.unwrap();

        assert_eq!(header[0], 0x81, "the channel sends final text frames");
        assert_eq!(header[1] & 0x80, 0, "the channel does not mask");

        let length = match header[1] & 0x7F {
            126 => {
                let mut extended = [0_u8; 2];

                self.socket.read_exact(&mut extended).await.unwrap();

                usize::from(u16::from_be_bytes(extended))
            }
            small => usize::from(small),
        };
        let mut payload = vec![0_u8; length];

        self.socket.read_exact(&mut payload).await.unwrap();

        String::from_utf8(payload).expect("a text frame is UTF-8")
    }
}

/// Reads an HTTP head, up to the blank line.
async fn read_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];

    loop {
        match socket.read(&mut byte).await {
            Ok(0) | Err(_) => break,
            Ok(_) => head.push(byte[0]),
        }

        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    String::from_utf8_lossy(&head).into_owned()
}
