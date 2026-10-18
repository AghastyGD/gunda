//! A minimal server-side WebSocket implementation.
//!
//! Only the subset the capture channel needs is implemented: the RFC 6455
//! opening handshake, text messages (fragmented or not), ping/pong, and close.
//! Binary messages and protocol extensions are refused rather than ignored,
//! because a capture client never sends them and dropping what we do not
//! understand is worse than failing the connection.
//!
//! A full WebSocket library would be a large dependency tree for a loopback
//! channel with exactly one client type, so this is deliberately small and
//! tested against the handshake example in RFC 6455 section 1.3.

use std::fmt;
use std::io;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The `Sec-WebSocket-Accept` GUID from RFC 6455 section 4.2.2.
const HANDSHAKE_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Largest request head accepted, in bytes.
const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;

/// Largest complete message accepted, in bytes.
///
/// The channel carries a handful of short JSON objects, so this is generous
/// while still bounding what one connection can make the process allocate.
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;

/// Frame opcodes from RFC 6455 section 5.2.
const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Failure while reading or writing a WebSocket connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSocketError {
    /// The transport failed.
    Io(io::ErrorKind),
    /// The opening handshake was not a valid WebSocket upgrade.
    Handshake,
    /// The peer violated the framing protocol.
    Protocol,
    /// A message exceeded [`MAX_MESSAGE_BYTES`].
    MessageTooLarge,
}

impl WebSocketError {
    fn io(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

impl fmt::Display for WebSocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(kind) => write!(f, "the connection failed ({kind:?})"),
            Self::Handshake => f.write_str("the WebSocket handshake was invalid"),
            Self::Protocol => f.write_str("the peer violated the WebSocket protocol"),
            Self::MessageTooLarge => f.write_str("the WebSocket message was too large"),
        }
    }
}

impl std::error::Error for WebSocketError {}

/// The opening handshake of an accepted connection.
///
/// The request path and origin are kept so the caller can decide what
/// connections to serve and reject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    path: String,
    origin: Option<String>,
}

impl Handshake {
    /// The request target of the upgrade request.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The `Origin` header, when the client sent one.
    #[must_use]
    pub fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }
}

/// Computes the `Sec-WebSocket-Accept` value for a client key.
#[must_use]
pub fn accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(HANDSHAKE_GUID.as_bytes());

    STANDARD.encode(hasher.finalize().as_slice())
}

/// A WebSocket connection over an unencrypted stream.
///
/// The transport is a `tokio` stream rather than a `TcpStream` so the same
/// code can be driven over an in-memory pipe in tests.
pub struct WebSocket<S> {
    stream: S,
}

impl<S> WebSocket<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Wraps an accepted stream.
    #[must_use]
    pub fn new(stream: S) -> Self {
        Self { stream }
    }

    /// Performs the opening handshake and answers it.
    pub async fn accept(&mut self) -> Result<Handshake, WebSocketError> {
        let head = self.read_head().await?;
        let request = parse_handshake(&head)?;

        self.stream
            .write_all(request.response.as_bytes())
            .await
            .map_err(WebSocketError::io)?;
        self.stream.flush().await.map_err(WebSocketError::io)?;

        Ok(request.handshake)
    }

    /// Reads the next text message, answering control frames as needed.
    ///
    /// Returns `None` when the peer closes the connection.
    pub async fn receive(&mut self) -> Result<Option<String>, WebSocketError> {
        let mut message: Vec<u8> = Vec::new();
        let mut started = false;

        loop {
            let (fin, opcode, payload) = self.read_frame().await?;

            match opcode {
                OP_PING => self.write_frame(OP_PONG, &payload).await?,
                OP_PONG => {}
                OP_CLOSE => {
                    let _ = self.write_frame(OP_CLOSE, &[]).await;

                    return Ok(None);
                }
                OP_TEXT => {
                    if started || !message.is_empty() {
                        return Err(WebSocketError::Protocol);
                    }

                    started = true;
                    push_bounded(&mut message, &payload)?;

                    if fin {
                        return decode(message);
                    }
                }
                OP_CONTINUATION => {
                    if !started {
                        return Err(WebSocketError::Protocol);
                    }

                    push_bounded(&mut message, &payload)?;

                    if fin {
                        return decode(message);
                    }
                }
                // Binary messages, reserved opcodes, and unknown opcodes are all
                // refused: this channel is text-only by construction, and
                // dropping a frame we do not understand would be worse than
                // ending the connection.
                _ => return Err(WebSocketError::Protocol),
            }
        }
    }

    /// Sends a text message as a single frame.
    pub async fn send_text(&mut self, message: &str) -> Result<(), WebSocketError> {
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(WebSocketError::MessageTooLarge);
        }

        self.write_frame(OP_TEXT, message.as_bytes()).await
    }

    /// Sends a close frame.
    pub async fn close(&mut self) -> Result<(), WebSocketError> {
        self.write_frame(OP_CLOSE, &[]).await
    }

    /// Reads the request head, up to the terminating blank line.
    ///
    /// The head is read one byte at a time so that whatever the client sent
    /// after it stays in the transport for the frame decoder.
    async fn read_head(&mut self) -> Result<String, WebSocketError> {
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];

        loop {
            if self
                .stream
                .read(&mut byte)
                .await
                .map_err(WebSocketError::io)?
                == 0
            {
                return Err(WebSocketError::Handshake);
            }

            head.push(byte[0]);

            if head.ends_with(b"\r\n\r\n") {
                break;
            }

            if head.len() > MAX_HANDSHAKE_BYTES {
                return Err(WebSocketError::Handshake);
            }
        }

        String::from_utf8(head).map_err(|_| WebSocketError::Handshake)
    }

    /// Reads one frame and unmasks its payload.
    async fn read_frame(&mut self) -> Result<(bool, u8, Vec<u8>), WebSocketError> {
        let mut header = [0_u8; 2];
        self.read_exact(&mut header).await?;

        // Reserved bits must be clear: no extension was negotiated.
        if header[0] & 0x70 != 0 {
            return Err(WebSocketError::Protocol);
        }

        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0F;

        // A client must mask every frame (RFC 6455 section 5.1).
        if header[1] & 0x80 == 0 {
            return Err(WebSocketError::Protocol);
        }

        let length = match header[1] & 0x7F {
            126 => {
                let mut extended = [0_u8; 2];
                self.read_exact(&mut extended).await?;
                u64::from(u16::from_be_bytes(extended))
            }
            127 => {
                let mut extended = [0_u8; 8];
                self.read_exact(&mut extended).await?;
                u64::from_be_bytes(extended)
            }
            small => u64::from(small),
        };

        // Control frames must be short and unfragmented (RFC 6455 section 5.5).
        if opcode >= OP_CLOSE && (!fin || length > 125) {
            return Err(WebSocketError::Protocol);
        }

        let length = usize::try_from(length).map_err(|_| WebSocketError::MessageTooLarge)?;

        if length > MAX_MESSAGE_BYTES {
            return Err(WebSocketError::MessageTooLarge);
        }

        let mut mask = [0_u8; 4];
        self.read_exact(&mut mask).await?;

        let mut payload = vec![0_u8; length];
        self.read_exact(&mut payload).await?;

        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }

        Ok((fin, opcode, payload))
    }

    /// Writes one unfragmented frame.
    async fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WebSocketError> {
        let mut frame = Vec::with_capacity(payload.len() + 10);
        frame.push(0x80 | opcode);

        match payload.len() {
            length if length < 126 => {
                frame.push(u8::try_from(length).map_err(|_| WebSocketError::MessageTooLarge)?)
            }
            length if length <= usize::from(u16::MAX) => {
                frame.push(126);
                frame.extend_from_slice(
                    &u16::try_from(length)
                        .map_err(|_| WebSocketError::MessageTooLarge)?
                        .to_be_bytes(),
                );
            }
            length => {
                frame.push(127);
                frame.extend_from_slice(
                    &u64::try_from(length)
                        .map_err(|_| WebSocketError::MessageTooLarge)?
                        .to_be_bytes(),
                );
            }
        }

        frame.extend_from_slice(payload);

        self.stream
            .write_all(&frame)
            .await
            .map_err(WebSocketError::io)?;
        self.stream.flush().await.map_err(WebSocketError::io)
    }

    async fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), WebSocketError> {
        self.stream
            .read_exact(buffer)
            .await
            .map_err(WebSocketError::io)?;

        Ok(())
    }
}

/// Refuses payloads that would push a message over the size limit.
fn push_bounded(message: &mut Vec<u8>, payload: &[u8]) -> Result<(), WebSocketError> {
    if message.len() + payload.len() > MAX_MESSAGE_BYTES {
        return Err(WebSocketError::MessageTooLarge);
    }

    message.extend_from_slice(payload);

    Ok(())
}

/// Validates a text message's UTF-8.
fn decode(message: Vec<u8>) -> Result<Option<String>, WebSocketError> {
    String::from_utf8(message)
        .map(Some)
        .map_err(|_| WebSocketError::Protocol)
}

/// A validated opening handshake and the response it requires.
struct HandshakeRequest {
    handshake: Handshake,
    response: String,
}

/// Parses an opening handshake and builds its response.
fn parse_handshake(head: &str) -> Result<HandshakeRequest, WebSocketError> {
    let mut lines = head.split("\r\n");

    let request_line = lines.next().ok_or(WebSocketError::Handshake)?;
    let mut fields = request_line.split(' ');

    let method = fields.next().ok_or(WebSocketError::Handshake)?;
    let path = fields.next().ok_or(WebSocketError::Handshake)?;

    if method != "GET" || !fields.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err(WebSocketError::Handshake);
    }

    let mut upgrade = false;
    let mut connection = false;
    let mut key = None;
    let mut version = false;
    let mut origin = None;

    for line in lines {
        if line.is_empty() {
            break;
        }

        let (name, value) = line.split_once(':').ok_or(WebSocketError::Handshake)?;
        let name = name.trim();
        let value = value.trim();

        if name.eq_ignore_ascii_case("upgrade") {
            upgrade = value.eq_ignore_ascii_case("websocket");
        } else if name.eq_ignore_ascii_case("connection") {
            connection = value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
        } else if name.eq_ignore_ascii_case("sec-websocket-key") {
            key = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("sec-websocket-version") {
            version = value == "13";
        } else if name.eq_ignore_ascii_case("origin") {
            origin = Some(value.to_owned());
        }
    }

    if !upgrade || !connection || !version {
        return Err(WebSocketError::Handshake);
    }

    let key = key
        .filter(|key| !key.is_empty())
        .ok_or(WebSocketError::Handshake)?;

    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(&key)
    );

    Ok(HandshakeRequest {
        handshake: Handshake {
            path: path.to_owned(),
            origin,
        },
        response,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

    use super::{MAX_MESSAGE_BYTES, WebSocket, WebSocketError, accept_key};

    const OP_TEXT: u8 = 0x1;
    const OP_BINARY: u8 = 0x2;
    const OP_CLOSE: u8 = 0x8;
    const OP_PING: u8 = 0x9;

    /// Keeps a broken implementation from hanging the suite.
    async fn within<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("the test must not block")
    }

    /// The handshake example from RFC 6455 section 1.3.
    #[test]
    fn accept_key_matches_the_rfc_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    fn client_handshake(path: &str, origin: Option<&str>) -> String {
        let mut head = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: 127.0.0.1\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n"
        );

        if let Some(origin) = origin {
            head.push_str(&format!("Origin: {origin}\r\n"));
        }

        head.push_str("\r\n");

        head
    }

    /// Encodes one client frame; client frames are always masked.
    fn client_frame(opcode: u8, payload: &[u8], fin: bool) -> Vec<u8> {
        let mask = [0x11_u8, 0x22, 0x33, 0x44];
        let mut frame = vec![if fin { 0x80 | opcode } else { opcode }];

        if payload.len() < 126 {
            frame.push(0x80 | u8::try_from(payload.len()).expect("length fits"));
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(
                &u16::try_from(payload.len())
                    .expect("length fits")
                    .to_be_bytes(),
            );
        }

        frame.extend_from_slice(&mask);

        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }

        frame
    }

    /// A frame header claiming a length, without the payload.
    fn claimed_frame(opcode: u8, length: u64) -> Vec<u8> {
        let mut frame = vec![0x80 | opcode, 0x80 | 127];
        frame.extend_from_slice(&length.to_be_bytes());

        frame
    }

    /// Reads a handshake response, one byte at a time so that nothing that
    /// follows it on the wire is swallowed.
    async fn read_response_head(client: &mut DuplexStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];

        loop {
            match within(client.read(&mut byte)).await {
                Ok(0) => break,
                Ok(_) => head.push(byte[0]),
                Err(error) => panic!("the response could not be read: {error}"),
            }

            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }

        String::from_utf8(head).expect("the response is text")
    }

    #[tokio::test]
    async fn accepts_a_handshake_and_reads_a_masked_message() {
        let (mut client, server) = duplex(64 * 1024);

        let response = within(async move {
            client
                .write_all(client_handshake("/ws", Some("chrome-extension://abc")).as_bytes())
                .await
                .expect("write");
            client
                .write_all(&client_frame(OP_TEXT, br#"{"kind":"hello"}"#, true))
                .await
                .expect("write");
            client.flush().await.expect("flush");

            let mut socket = WebSocket::new(server);
            let handshake = socket.accept().await.expect("the handshake must succeed");
            let message = socket.receive().await.expect("the message must be read");

            let head = read_response_head(&mut client).await;

            (head, handshake, message)
        })
        .await;

        assert!(
            response
                .0
                .contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
        assert_eq!(response.1.path(), "/ws");
        assert_eq!(response.1.origin(), Some("chrome-extension://abc"));
        assert_eq!(response.2.as_deref(), Some(r#"{"kind":"hello"}"#));
    }

    #[tokio::test]
    async fn reassembles_a_fragmented_message() {
        let (mut client, server) = duplex(64 * 1024);

        let message = within(async {
            client
                .write_all(client_handshake("/ws", None).as_bytes())
                .await
                .expect("write");
            client
                .write_all(&client_frame(OP_TEXT, b"{\"a\":", false))
                .await
                .expect("write");
            client
                .write_all(&client_frame(0x0, b"1}", true))
                .await
                .expect("write");
            client.flush().await.expect("flush");

            let mut socket = WebSocket::new(server);
            socket.accept().await.expect("the handshake must succeed");

            socket.receive().await.expect("the message must be read")
        })
        .await;

        assert_eq!(message.as_deref(), Some(r#"{"a":1}"#));
    }

    #[tokio::test]
    async fn answers_a_ping_before_the_next_message() {
        let (mut client, server) = duplex(64 * 1024);

        let pong = within(async move {
            client
                .write_all(client_handshake("/ws", None).as_bytes())
                .await
                .expect("write");
            client
                .write_all(&client_frame(OP_PING, b"are you there", true))
                .await
                .expect("write");
            client
                .write_all(&client_frame(OP_TEXT, b"next", true))
                .await
                .expect("write");
            client.flush().await.expect("flush");

            let mut socket = WebSocket::new(server);
            socket.accept().await.expect("the handshake must succeed");

            let message = socket.receive().await.expect("the message must be read");

            assert_eq!(message.as_deref(), Some("next"));

            // The response head is written during the handshake, so it is
            // waiting on the wire ahead of the pong.
            let head = read_response_head(&mut client).await;

            assert!(head.starts_with("HTTP/1.1 101"), "unexpected: {head}");

            let mut pong = [0_u8; 15];
            client.read_exact(&mut pong).await.expect("read");

            pong
        })
        .await;

        assert_eq!(pong[0], 0x8A, "a pong frame with FIN set");
        assert_eq!(pong[1], 13, "an unmasked thirteen byte payload");
        assert_eq!(&pong[2..], b"are you there", "the payload is echoed");
    }

    #[tokio::test]
    async fn refuses_an_unmasked_frame() {
        let (mut client, server) = duplex(64 * 1024);

        let error = within(async {
            client
                .write_all(client_handshake("/ws", None).as_bytes())
                .await
                .expect("write");
            // A well-formed frame, except that clients must mask.
            client
                .write_all(&[0x81, 0x02, b'h', b'i'])
                .await
                .expect("write");
            client.flush().await.expect("flush");

            let mut socket = WebSocket::new(server);
            socket.accept().await.expect("the handshake must succeed");

            socket
                .receive()
                .await
                .expect_err("the frame must be refused")
        })
        .await;

        assert_eq!(error, WebSocketError::Protocol);
    }

    #[tokio::test]
    async fn refuses_a_binary_message() {
        let (mut client, server) = duplex(64 * 1024);

        let error = within(async {
            client
                .write_all(client_handshake("/ws", None).as_bytes())
                .await
                .expect("write");
            client
                .write_all(&client_frame(OP_BINARY, &[1, 2, 3], true))
                .await
                .expect("write");
            client.flush().await.expect("flush");

            let mut socket = WebSocket::new(server);
            socket.accept().await.expect("the handshake must succeed");

            socket
                .receive()
                .await
                .expect_err("the frame must be refused")
        })
        .await;

        assert_eq!(error, WebSocketError::Protocol);
    }

    #[tokio::test]
    async fn refuses_an_oversized_message_without_buffering_it() {
        let (mut client, server) = duplex(64 * 1024);

        let error = within(async {
            client
                .write_all(client_handshake("/ws", None).as_bytes())
                .await
                .expect("write");
            client
                .write_all(&claimed_frame(OP_TEXT, MAX_MESSAGE_BYTES as u64 + 1))
                .await
                .expect("write");
            client.flush().await.expect("flush");

            let mut socket = WebSocket::new(server);
            socket.accept().await.expect("the handshake must succeed");

            socket
                .receive()
                .await
                .expect_err("the frame must be refused")
        })
        .await;

        assert_eq!(error, WebSocketError::MessageTooLarge);
    }

    #[tokio::test]
    async fn refuses_an_incomplete_handshake() {
        let (mut client, server) = duplex(64 * 1024);

        let error = within(async {
            client
                .write_all(
                    b"GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\
                      Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\r\n",
                )
                .await
                .expect("write");
            client.flush().await.expect("flush");

            let mut socket = WebSocket::new(server);

            socket
                .accept()
                .await
                .expect_err("the handshake must be refused")
        })
        .await;

        assert_eq!(error, WebSocketError::Handshake);
    }

    #[tokio::test]
    async fn reports_a_closed_connection() {
        let (mut client, server) = duplex(64 * 1024);

        let message = within(async {
            client
                .write_all(client_handshake("/ws", None).as_bytes())
                .await
                .expect("write");
            client
                .write_all(&client_frame(OP_CLOSE, &1000_u16.to_be_bytes(), true))
                .await
                .expect("write");
            client.flush().await.expect("flush");

            let mut socket = WebSocket::new(server);
            socket.accept().await.expect("the handshake must succeed");

            socket.receive().await.expect("a close is not an error")
        })
        .await;

        assert_eq!(message, None);
    }
}
