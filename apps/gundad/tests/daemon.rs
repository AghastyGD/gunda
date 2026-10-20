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
