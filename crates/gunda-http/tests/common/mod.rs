use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use url::Url;

pub async fn serve_once(response: &'static str) -> (Url, JoinHandle<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("local listener must bind");

    let address = listener.local_addr().expect("listener address must exist");

    let task = tokio::spawn(async move {
        timeout(Duration::from_secs(10), async move {
            let (mut socket, _) = listener.accept().await.expect("client must connect");

            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];

            loop {
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("request must be readable");

                assert_ne!(read, 0, "client closed before sending headers");
                request.extend_from_slice(&buffer[..read]);

                assert!(request.len() <= 16 * 1024, "test request is too large");

                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }

            socket
                .write_all(response.as_bytes())
                .await
                .expect("response must be written");

            socket.shutdown().await.expect("socket must shut down");

            String::from_utf8(request).expect("test request must be UTF-8")
        })
        .await
        .expect("local server must finish within its deadline")
    });

    let url = Url::parse(&format!("http://{address}/file.bin")).expect("local URL must be valid");

    (url, task)
}
