use std::time::Duration;

use gunda_capture::test_client::TestClient;
use gunda_capture::{endpoint_path, load_token, token_path};
use gunda_ipc::{
    CaptureTrackChoice, ClientMessage, Command, Connection, NativePath, ServerMessage, Snapshot,
    connect, read_endpoint,
};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread")]
async fn capture_continues_while_no_client_is_connected() {
    let directory = tempdir().unwrap();
    let (address, server) = file_server(b"hello").await;
    let daemon = tokio::spawn(gundad::run(directory.path().to_path_buf()));
    let endpoint = wait_for_endpoint(directory.path()).await;
    let token = load_token(&directory.path().join("gundad-token"))
        .unwrap()
        .unwrap();
    let (client, initial) = connect(&endpoint, token.as_str()).await.unwrap();
    let capture = initial.capture.expect("capture starts with the daemon");
    let published_endpoint = std::fs::read_to_string(endpoint_path(directory.path())).unwrap();
    let published_token = load_token(&token_path(directory.path())).unwrap().unwrap();
    assert_eq!(published_endpoint.trim(), capture.endpoint);
    assert_eq!(published_token.as_str(), capture.token);
    drop(client);

    let mut extension = TestClient::connect(capture_address(&capture.endpoint), "/capture", None)
        .await
        .0;
    extension
        .send(&format!(
            r#"{{"kind":"hello","version":3,"token":"{}","agent":"gundad-test"}}"#,
            capture.token
        ))
        .await;
    assert!(
        extension
            .receive()
            .await
            .unwrap()
            .contains(r#""kind":"ready""#)
    );
    extension
        .send(&format!(
            r#"{{"kind":"candidate","url":"http://{address}/file.bin","content_type":"video/mp4","page_url":"https://example.test/watch","page_title":"A movie"}}"#
        ))
        .await;
    assert!(
        extension
            .receive()
            .await
            .unwrap()
            .contains(r#""kind":"accepted""#)
    );

    let (mut client, _initial) = connect(&endpoint, token.as_str()).await.unwrap();
    let captured = wait_for_capture(&mut client).await;
    let capture = &captured.captures[0];

    assert_eq!(capture.kind, "File");
    assert_eq!(capture.page_title.as_deref(), Some("A movie"));

    client
        .send(&ClientMessage::Command {
            command: Command::DownloadCapture {
                id: capture.id,
                directory: NativePath::from_path(directory.path()).unwrap(),
                track: None,
            },
        })
        .await
        .unwrap();

    let completed = wait_for_completed(&mut client).await;
    let job = completed
        .jobs
        .iter()
        .find(|job| job.state == "completed")
        .unwrap();
    let output = job.output.as_ref().unwrap().to_path().unwrap();

    assert_eq!(job.origin, "Browser");
    assert_eq!(
        output.file_name().and_then(|name| name.to_str()),
        Some("A movie.mp4")
    );
    server.await.unwrap();
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_downloads_a_generic_file_with_its_suggested_name() {
    let directory = tempdir().unwrap();
    let (address, server) = typed_file_server(b"%PDF-1.7\nreport", "application/pdf").await;
    let daemon = tokio::spawn(gundad::run(directory.path().to_path_buf()));
    let endpoint = wait_for_endpoint(directory.path()).await;
    let token = load_token(&directory.path().join("gundad-token"))
        .unwrap()
        .unwrap();
    let (mut client, initial) = connect(&endpoint, token.as_str()).await.unwrap();
    let capture = initial.capture.expect("capture starts with the daemon");
    let mut extension = TestClient::connect(capture_address(&capture.endpoint), "/capture", None)
        .await
        .0;
    extension
        .send(&format!(
            r#"{{"kind":"hello","version":3,"token":"{}","agent":"gundad-test"}}"#,
            capture.token
        ))
        .await;
    assert!(
        extension
            .receive()
            .await
            .unwrap()
            .contains(r#""kind":"ready""#)
    );
    extension
        .send(&format!(
            r#"{{"kind":"candidate","url":"http://{address}/download","content_type":"application/pdf","page_url":"https://example.test/files","page_title":"Files","filename":"Quarterly report.pdf"}}"#
        ))
        .await;
    assert!(
        extension
            .receive()
            .await
            .unwrap()
            .contains(r#""kind":"accepted""#)
    );

    let captured = wait_for_capture(&mut client).await;
    let capture = &captured.captures[0];

    assert_eq!(capture.kind, "File");
    assert_eq!(capture.filename.as_deref(), Some("Quarterly report.pdf"));
    assert_eq!(capture.page_title.as_deref(), Some("Files"));

    client
        .send(&ClientMessage::Command {
            command: Command::DownloadCapture {
                id: capture.id,
                directory: NativePath::from_path(directory.path()).unwrap(),
                track: None,
            },
        })
        .await
        .unwrap();

    let completed = wait_for_completed(&mut client).await;
    let job = completed
        .jobs
        .iter()
        .find(|job| job.state == "completed")
        .unwrap();
    let output = job.output.as_ref().unwrap().to_path().unwrap();

    assert_eq!(job.origin, "Browser");
    assert_eq!(
        output.file_name().and_then(|name| name.to_str()),
        Some("Quarterly report.pdf")
    );
    assert_eq!(std::fs::read(&output).unwrap(), b"%PDF-1.7\nreport");
    server.await.unwrap();
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_browser_download_is_queued_and_named_by_the_browser() {
    let directory = tempdir().unwrap();
    let (address, server) = typed_file_server(b"%PDF-1.7\nbrowser", "application/pdf").await;
    let daemon = tokio::spawn(gundad::run(directory.path().to_path_buf()));
    let endpoint = wait_for_endpoint(directory.path()).await;
    let token = load_token(&directory.path().join("gundad-token"))
        .unwrap()
        .unwrap();
    let (mut client, initial) = connect(&endpoint, token.as_str()).await.unwrap();
    let capture = initial.capture.expect("capture starts with the daemon");
    let mut extension = TestClient::connect(capture_address(&capture.endpoint), "/capture", None)
        .await
        .0;
    extension
        .send(&format!(
            r#"{{"kind":"hello","version":3,"token":"{}","agent":"gundad-test"}}"#,
            capture.token
        ))
        .await;
    assert!(
        extension
            .receive()
            .await
            .unwrap()
            .contains(r#""kind":"ready""#)
    );
    extension
        .send(
            &serde_json::json!({
                "kind": "download_browser",
                "url": format!("http://{address}/download"),
                "content_type": "application/pdf",
                "page_url": "https://example.test/files",
                "page_title": "Files",
                "filename": "Browser report.pdf",
                "directory": directory.path().display().to_string(),
            })
            .to_string(),
        )
        .await;
    assert!(
        extension
            .receive()
            .await
            .unwrap()
            .contains(r#""kind":"download_queued""#)
    );

    let completed = wait_for_completed(&mut client).await;
    let job = completed
        .jobs
        .iter()
        .find(|job| job.state == "completed")
        .unwrap();
    let output = job.output.as_ref().unwrap().to_path().unwrap();

    assert_eq!(job.origin, "Browser");
    assert_eq!(
        output.file_name().and_then(|name| name.to_str()),
        Some("Browser report.pdf")
    );
    assert_eq!(std::fs::read(&output).unwrap(), b"%PDF-1.7\nbrowser");
    server.await.unwrap();
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_inspects_the_qualities_of_a_capture() {
    const MASTER: &str = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\n\
low.m3u8\n";
    let directory = tempdir().unwrap();
    let (base, server) = playlist_server(MASTER).await;
    let daemon = tokio::spawn(gundad::run(directory.path().to_path_buf()));
    let endpoint = wait_for_endpoint(directory.path()).await;
    let token = load_token(&directory.path().join("gundad-token"))
        .unwrap()
        .unwrap();
    let (mut client, initial) = connect(&endpoint, token.as_str()).await.unwrap();
    let capture = initial.capture.expect("capture starts with the daemon");
    let mut extension = TestClient::connect(capture_address(&capture.endpoint), "/capture", None)
        .await
        .0;
    extension
        .send(&format!(
            r#"{{"kind":"hello","version":3,"token":"{}","agent":"gundad-test"}}"#,
            capture.token
        ))
        .await;
    assert!(
        extension
            .receive()
            .await
            .unwrap()
            .contains(r#""kind":"ready""#)
    );
    extension
        .send(&format!(
            r#"{{"kind":"candidate","url":"{}","content_type":"application/vnd.apple.mpegurl","page_url":"https://example.test/watch","page_title":"A presentation"}}"#,
            base.join("master.m3u8").unwrap()
        ))
        .await;
    assert!(
        extension
            .receive()
            .await
            .unwrap()
            .contains(r#""kind":"accepted""#)
    );

    let captured = wait_for_capture(&mut client).await;
    client
        .send(&ClientMessage::Command {
            command: Command::InspectCapture {
                id: captured.captures[0].id,
            },
        })
        .await
        .unwrap();

    let inspected = wait_for_tracks(&mut client).await;
    let tracks = inspected.capture_tracks.unwrap();

    assert_eq!(tracks.problem, None);
    assert_eq!(tracks.tracks.len(), 1);
    assert!(tracks.tracks[0].label.contains("640x360"));
    assert!(tracks.tracks[0].details.starts_with("video | "));

    server.await.unwrap();
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_downloads_a_selected_progressive_source_variant() {
    let directory = tempdir().unwrap();
    let (address, server) = file_server(b"high quality video").await;
    let daemon = tokio::spawn(gundad::run(directory.path().to_path_buf()));
    let endpoint = wait_for_endpoint(directory.path()).await;
    let token = load_token(&directory.path().join("gundad-token"))
        .unwrap()
        .unwrap();
    let (mut client, initial) = connect(&endpoint, token.as_str()).await.unwrap();
    let capture = initial.capture.expect("capture starts with the daemon");
    let mut extension = TestClient::connect(capture_address(&capture.endpoint), "/capture", None)
        .await
        .0;
    extension
        .send(&format!(
            r#"{{"kind":"hello","version":3,"token":"{}","agent":"gundad-test"}}"#,
            capture.token
        ))
        .await;
    extension.receive().await.unwrap();

    let source = format!(
        r#"{{"kind":"source","source_url":"https://www.instagram.com/api/graphql","page_url":"https://www.instagram.com/reels/example/","page_title":"A reel","document":{{"media":{{"pk":"7","video_versions":[{{"url":"https://cdn.example/low.mp4","width":640,"height":360}},{{"url":"http://{address}/high.mp4","width":1080,"height":1920}}]}}}}}}"#
    );
    extension.send(&source).await;
    assert!(
        extension
            .receive()
            .await
            .unwrap()
            .contains(r#""kind":"recorded""#)
    );

    let captured = wait_for_capture(&mut client).await;
    let capture = &captured.captures[0];
    assert!(capture.selectable);

    extension
        .send(r#"{"kind":"list_formats","page_url":"https://www.instagram.com/reels/example/"}"#)
        .await;
    let formats = extension.receive().await.unwrap();
    assert!(formats.contains(r#""kind":"page_formats""#));
    assert!(formats.contains(r#"variant:1"#));
    assert!(formats.contains(r#"1080x1920"#));

    client
        .send(&ClientMessage::Command {
            command: Command::InspectCapture { id: capture.id },
        })
        .await
        .unwrap();
    let inspected = wait_for_tracks(&mut client).await;
    let tracks = inspected.capture_tracks.unwrap().tracks;
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].label, "1080x1920");
    let selected = tracks[0].clone();

    client
        .send(&ClientMessage::Command {
            command: Command::DownloadCapture {
                id: capture.id,
                directory: NativePath::from_path(directory.path()).unwrap(),
                track: Some(CaptureTrackChoice {
                    url: selected.url,
                    filename: selected.filename,
                    selection: selected.selection,
                }),
            },
        })
        .await
        .unwrap();

    let completed = wait_for_completed(&mut client).await;
    let job = completed
        .jobs
        .iter()
        .find(|job| job.state == "completed")
        .unwrap();
    let output = job.output.as_ref().unwrap().to_path().unwrap();
    assert_eq!(
        output.file_name().and_then(|name| name.to_str()),
        Some("A reel.mp4")
    );

    server.await.unwrap();
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn player_button_starts_a_named_download_and_reports_its_status() {
    let directory = tempdir().unwrap();
    let (address, server) = file_server(b"from the player button").await;
    let daemon = tokio::spawn(gundad::run(directory.path().to_path_buf()));
    let endpoint = wait_for_endpoint(directory.path()).await;
    let token = load_token(&directory.path().join("gundad-token"))
        .unwrap()
        .unwrap();
    let (mut client, initial) = connect(&endpoint, token.as_str()).await.unwrap();
    let capture = initial.capture.expect("capture starts with the daemon");
    let mut extension = TestClient::connect(capture_address(&capture.endpoint), "/capture", None)
        .await
        .0;
    extension
        .send(&format!(
            r#"{{"kind":"hello","version":3,"token":"{}","agent":"gundad-test"}}"#,
            capture.token
        ))
        .await;
    extension.receive().await.unwrap();
    extension
        .send(&format!(
            r#"{{"kind":"candidate","url":"http://{address}/player.mp4","content_type":"video/mp4","page_url":"https://example.test/watch","page_title":"A player video"}}"#
        ))
        .await;
    assert!(
        extension
            .receive()
            .await
            .unwrap()
            .contains(r#""kind":"accepted""#)
    );

    wait_for_capture(&mut client).await;
    extension
        .send(r#"{"kind":"list_formats","page_url":"https://example.test/watch"}"#)
        .await;
    let formats = extension.receive().await.unwrap();
    let capture_id = formats
        .split(r#""capture_id":"#)
        .nth(1)
        .and_then(|value| value.split([',', '}']).next())
        .and_then(|value| value.parse::<u64>().ok())
        .expect("the formats response names the capture");
    let download = serde_json::json!({
        "kind": "download_page",
        "page_url": "https://example.test/watch",
        "capture_id": capture_id,
        "directory": directory.path().to_string_lossy(),
        "filename": "Chosen name.mp4",
    });
    extension.send(&download.to_string()).await;
    let queued = extension.receive().await.unwrap();
    assert!(queued.contains(r#""kind":"download_queued""#));
    let download_id = queued
        .split(r#""id":"#)
        .nth(1)
        .and_then(|value| value.trim_end_matches('}').parse::<u64>().ok())
        .expect("the queued response names the download");
    extension
        .send(&format!(
            r#"{{"kind":"download_status","id":{download_id}}}"#
        ))
        .await;
    let status = extension.receive().await.unwrap();
    assert!(status.contains(r#""kind":"download_status""#));
    assert!(status.contains(r#""name":"Chosen name.mp4""#));

    let completed = wait_for_completed(&mut client).await;
    let output = completed
        .jobs
        .iter()
        .find(|job| job.state == "completed")
        .and_then(|job| job.output.as_ref())
        .expect("the player download is published")
        .to_path()
        .unwrap();

    assert_eq!(std::fs::read(output).unwrap(), b"from the player button");
    server.await.unwrap();
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn connected_clients_share_a_job_and_disconnect_does_not_cancel_it() {
    let directory = tempdir().unwrap();
    let (address, server) = slow_server().await;
    let daemon = tokio::spawn(gundad::run(directory.path().to_path_buf()));
    let endpoint = wait_for_endpoint(directory.path()).await;
    let token = load_token(&directory.path().join("gundad-token"))
        .unwrap()
        .unwrap();
    let (mut first, _initial) = connect(&endpoint, token.as_str()).await.unwrap();
    let (mut second, _initial) = connect(&endpoint, token.as_str()).await.unwrap();

    first
        .send(&ClientMessage::Command {
            command: Command::Create {
                url: format!("http://{address}/slow.bin"),
                directory: NativePath::from_path(directory.path()).unwrap(),
                preferred_filename: Some("slow.bin".into()),
                selection: None,
                page_url: None,
                page_title: None,
                engine: Some("http".into()),
            },
        })
        .await
        .unwrap();

    let first_snapshot = wait_for_state(&mut first, "downloading").await;
    let second_snapshot = wait_for_state(&mut second, "downloading").await;
    let first_job = first_snapshot
        .jobs
        .iter()
        .find(|job| job.state == "downloading")
        .unwrap();
    let second_job = second_snapshot
        .jobs
        .iter()
        .find(|job| job.id == first_job.id)
        .unwrap();

    assert_eq!(second_job.state, "downloading");
    assert!(first_snapshot.active.contains(&first_job.id));
    assert!(second_snapshot.active.contains(&first_job.id));

    drop(first);

    let completed = wait_for_completed(&mut second).await;
    assert!(completed.jobs.iter().any(|job| job.id == first_job.id));

    server.await.unwrap();
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_accepts_a_job_and_reports_completion() {
    let directory = tempdir().unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello")
            .await
            .unwrap();
    });

    let daemon = tokio::spawn(gundad::run(directory.path().to_path_buf()));
    let endpoint = wait_for_endpoint(directory.path()).await;
    let token = load_token(&directory.path().join("gundad-token"))
        .unwrap()
        .unwrap();
    let (mut connection, _initial) = connect(&endpoint, token.as_str()).await.unwrap();
    connection
        .send(&ClientMessage::Command {
            command: Command::Create {
                url: format!("http://{address}/file.bin"),
                directory: NativePath::from_path(directory.path()).unwrap(),
                preferred_filename: Some("file.bin".into()),
                selection: None,
                page_url: None,
                page_title: None,
                engine: Some("http".into()),
            },
        })
        .await
        .unwrap();

    let completed = wait_for_completed(&mut connection).await;
    assert!(completed.jobs.iter().any(|job| job.state == "completed"));

    server.await.unwrap();
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_rejects_a_client_with_the_wrong_token() {
    let directory = tempdir().unwrap();
    let daemon = tokio::spawn(gundad::run(directory.path().to_path_buf()));
    let endpoint = wait_for_endpoint(directory.path()).await;

    let error = match connect(&endpoint, &"0".repeat(64)).await {
        Ok(_) => panic!("the daemon must reject the wrong token"),
        Err(error) => error,
    };

    assert_eq!(error, gunda_ipc::IpcError::Unauthorized);
    daemon.abort();
}

async fn wait_for_endpoint(directory: &std::path::Path) -> String {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(endpoint) = read_endpoint(directory)
                && !endpoint.is_empty()
            {
                return endpoint;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("daemon endpoint must appear")
}

async fn wait_for_completed(connection: &mut Connection) -> Snapshot {
    timeout(Duration::from_secs(5), async {
        loop {
            match connection.receive::<ServerMessage>().await.unwrap() {
                ServerMessage::Snapshot { snapshot }
                    if snapshot.jobs.iter().any(|job| job.state == "completed") =>
                {
                    return snapshot;
                }
                ServerMessage::Snapshot { .. } => {}
                ServerMessage::Error { message } => panic!("daemon error: {message}"),
            }
        }
    })
    .await
    .expect("daemon must finish the job")
}

async fn wait_for_capture(connection: &mut Connection) -> Snapshot {
    timeout(Duration::from_secs(5), async {
        loop {
            match connection.receive::<ServerMessage>().await.unwrap() {
                ServerMessage::Snapshot { snapshot } if !snapshot.captures.is_empty() => {
                    return snapshot;
                }
                ServerMessage::Snapshot { .. } => {}
                ServerMessage::Error { message } => panic!("daemon error: {message}"),
            }
        }
    })
    .await
    .expect("daemon must report the capture")
}

async fn wait_for_tracks(connection: &mut Connection) -> Snapshot {
    timeout(Duration::from_secs(5), async {
        loop {
            match connection.receive::<ServerMessage>().await.unwrap() {
                ServerMessage::Snapshot { snapshot } if snapshot.capture_tracks.is_some() => {
                    return snapshot;
                }
                ServerMessage::Snapshot { .. } => {}
                ServerMessage::Error { message } => panic!("daemon error: {message}"),
            }
        }
    })
    .await
    .expect("daemon must report the inspected tracks")
}

async fn wait_for_state(connection: &mut Connection, state: &str) -> Snapshot {
    timeout(Duration::from_secs(5), async {
        loop {
            match connection.receive::<ServerMessage>().await.unwrap() {
                ServerMessage::Snapshot { snapshot }
                    if snapshot.jobs.iter().any(|job| job.state == state) =>
                {
                    return snapshot;
                }
                ServerMessage::Snapshot { .. } => {}
                ServerMessage::Error { message } => panic!("daemon error: {message}"),
            }
        }
    })
    .await
    .expect("daemon must report the expected job state")
}

fn capture_address(endpoint: &str) -> std::net::SocketAddr {
    endpoint
        .strip_prefix("ws://")
        .and_then(|endpoint| endpoint.split('/').next())
        .unwrap()
        .parse()
        .unwrap()
}

async fn file_server(body: &'static [u8]) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    typed_file_server(body, "video/mp4").await
}

async fn typed_file_server(
    body: &'static [u8],
    content_type: &'static str,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: {content_type}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(body).await.unwrap();
    });
    (address, server)
}

async fn playlist_server(master: &'static str) -> (url::Url, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let base = url::Url::parse(&format!("http://{address}/")).unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/vnd.apple.mpegurl\r\nconnection: close\r\n\r\n{master}",
                    master.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    (base, server)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

async fn slow_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-length: 16\r\nconnection: close\r\n\r\nfirst ha",
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        socket.write_all(b"lf half!").await.unwrap();
    });
    (address, server)
}
