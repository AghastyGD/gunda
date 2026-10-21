use std::time::Duration;

use gunda_capture::load_token;
use gunda_ipc::{
    ClientMessage, Command, Connection, NativePath, ServerMessage, Snapshot, connect, read_endpoint,
};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

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
