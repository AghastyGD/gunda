//! A minimal WebSocket client for capture integration tests.
//!
//! The test client masks frames as a browser does, so integrations exercise the
//! same wire behavior as the extension.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[cfg(test)]
use crate::websocket::accept_key;

/// The key from the handshake example in RFC 6455 section 1.3.
const CLIENT_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// A connected test client.
pub struct TestClient {
    stream: TcpStream,
}

impl TestClient {
    /// Connects and completes the opening handshake.
    ///
    /// The response head is returned rather than asserted on, so a test can also
    /// check the paths that are refused.
    pub async fn connect(address: SocketAddr, path: &str, origin: Option<&str>) -> (Self, String) {
        let mut stream = TcpStream::connect(address)
            .await
            .expect("the listener is reachable");

        let mut request = format!(
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {CLIENT_KEY}\r\nSec-WebSocket-Version: 13\r\n"
        );

        if let Some(origin) = origin {
            request.push_str(&format!("Origin: {origin}\r\n"));
        }

        request.push_str("\r\n");

        stream
            .write_all(request.as_bytes())
            .await
            .expect("the request is written");
        stream.flush().await.expect("the request is flushed");

        let head = read_head(&mut stream).await;

        (Self { stream }, head)
    }

    /// Sends a text message as one masked frame.
    pub async fn send(&mut self, message: &str) {
        let frame = masked(0x1, message.as_bytes()).expect("the message fits one frame");

        self.stream
            .write_all(&frame)
            .await
            .expect("the frame is written");
        self.stream.flush().await.expect("the frame is flushed");
    }

    /// Reads one text message.
    ///
    /// Returns `None` when the connection is closed or broken, which is what a
    /// refusal looks like from this side.
    pub async fn receive(&mut self) -> Option<String> {
        let mut header = [0_u8; 2];

        if self.stream.read_exact(&mut header).await.is_err() {
            return None;
        }

        assert_eq!(header[0] & 0x70, 0, "no extension was negotiated");
        assert_eq!(header[0] & 0x80, 0x80, "a server frame is final");
        assert_eq!(header[1] & 0x80, 0, "a server frame is not masked");

        let length = match header[1] & 0x7F {
            126 => {
                let mut extended = [0_u8; 2];

                if self.stream.read_exact(&mut extended).await.is_err() {
                    return None;
                }

                usize::from(u16::from_be_bytes(extended))
            }
            127 => panic!("the server does not send frames this large"),
            small => usize::from(small),
        };

        let mut payload = vec![0_u8; length];

        if self.stream.read_exact(&mut payload).await.is_err() {
            return None;
        }

        let opcode = header[0] & 0x0F;

        assert!(
            opcode == 0x1 || opcode == 0x8,
            "the server sends text or close frames, not {opcode:#x}"
        );

        if opcode == 0x8 {
            return None;
        }

        Some(String::from_utf8(payload).expect("a text frame is UTF-8"))
    }
}

/// Encodes a masked client frame.
fn masked(opcode: u8, payload: &[u8]) -> Result<Vec<u8>, ()> {
    let mut frame = vec![0x80 | opcode];

    match payload.len() {
        length if length < 126 => frame.push(0x80 | u8::try_from(length).map_err(|_| ())?),
        length => {
            let length = u16::try_from(length).map_err(|_| ())?;

            frame.push(0x80 | 126);
            frame.extend_from_slice(&length.to_be_bytes());
        }
    }

    let mask = [0x51_u8, 0x62, 0x73, 0x84];
    frame.extend_from_slice(&mask);

    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[index % 4]);
    }

    Ok(frame)
}

/// Reads a response head, up to the terminating blank line.
async fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];

    loop {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => break,
            Ok(_) => head.push(byte[0]),
        }

        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    String::from_utf8_lossy(&head).into_owned()
}

/// A test that the handshake response is built the way the RFC describes it.
#[cfg(test)]
#[test]
fn accept_key_is_the_one_the_server_will_send() {
    assert_eq!(accept_key(CLIENT_KEY), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
}
