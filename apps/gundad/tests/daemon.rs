use std::time::Duration;

use gunda_capture::load_token;
use gunda_capture::test_client::TestClient;
use gunda_ipc::{
    ClientMessage, Command, Connection, NativePath, ServerMessage, Snapshot, connect, read_endpoint,
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
    drop(client);

    let mut extension = TestClient::connect(capture_address(&capture.endpoint), "/capture", None)
        .await
        .0;
    extension
        .send(&format!(
            r#"{{"kind":"hello","version":1,"token":"{}","agent":"gundad-test"}}"#,
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
async fn daemon_inspects_the_qualities_of_a_capture() {
    const MASTER: &str = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\n\
low.m3u8\n";
    const VARIANT: &str = "#EXTM3U\n\
#EXT-X-MAP:URI=\"init.mp4\"\n\
#EXTINF:4,\n\
s0.m4s\n\
#EXT-X-ENDLIST\n";

    let directory = tempdir().unwrap();
    let (base, server) = playlist_server(MASTER, VARIANT).await;
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
            r#"{{"kind":"hello","version":1,"token":"{}","agent":"gundad-test"}}"#,
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
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: video/mp4\r\nconnection: close\r\n\r\n",
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

async fn playlist_server(
    master: &'static str,
    variant: &'static str,
) -> (url::Url, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let base = url::Url::parse(&format!("http://{address}/")).unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let body = if request.contains("GET /master.m3u8") {
                master
            } else {
                variant
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/vnd.apple.mpegurl\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
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
