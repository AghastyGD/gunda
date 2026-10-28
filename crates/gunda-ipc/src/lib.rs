//! Versioned local IPC for the Gunda daemon and its clients.

use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::TcpStream;

/// Version of the daemon protocol.
pub const PROTOCOL_VERSION: u32 = 3;

/// Largest single protocol message accepted.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Browser native-messaging host name.
pub const NATIVE_HOST_NAME: &str = "com.gunda.capture";

/// Version of the browser native-messaging protocol.
pub const NATIVE_HOST_PROTOCOL_VERSION: u32 = 1;

/// Largest browser native-messaging request or response.
pub const MAX_NATIVE_HOST_MESSAGE_BYTES: usize = 1024 * 1024;

/// A request from the browser extension to its native host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NativeHostRequest {
    Pairing { version: u32, agent: String },
    ChooseDirectory { version: u32 },
}

/// A response from the native host to the browser extension.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NativeHostResponse {
    Pairing {
        version: u32,
        endpoint: String,
        token: String,
    },
    Directory {
        path: String,
    },
    Error {
        message: String,
    },
}

/// A path encoded without assuming it is UTF-8.
///
/// The daemon and its clients run on the same operating system. Windows paths
/// travel as UTF-16 bytes so a non-Unicode path is not silently changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePath {
    bytes: Vec<u8>,
}

impl NativePath {
    pub fn from_path(path: &Path) -> Result<Self, IpcError> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;

            return Ok(Self {
                bytes: path.as_os_str().as_bytes().to_vec(),
            });
        }

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt as _;

            return Ok(Self {
                bytes: path
                    .as_os_str()
                    .encode_wide()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            });
        }

        #[allow(unreachable_code)]
        path.to_str()
            .map(|path| Self {
                bytes: path.as_bytes().to_vec(),
            })
            .ok_or(IpcError::InvalidPath)
    }

    pub fn to_path(&self) -> Result<PathBuf, IpcError> {
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt as _;

            return Ok(PathBuf::from(OsString::from_vec(self.bytes.clone())));
        }

        #[cfg(windows)]
        {
            use std::ffi::OsString;
            use std::os::windows::ffi::OsStringExt as _;

            let wide: Vec<u16> = self
                .bytes
                .chunks_exact(2)
                .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
                .collect();

            return Ok(PathBuf::from(OsString::from_wide(&wide)));
        }

        #[allow(unreachable_code)]
        String::from_utf8(self.bytes.clone())
            .map(PathBuf::from)
            .map_err(|_| IpcError::InvalidPath)
    }
}

/// A command sent by a client to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Create {
        url: String,
        directory: NativePath,
        preferred_filename: Option<String>,
        selection: Option<String>,
        page_url: Option<String>,
        page_title: Option<String>,
        engine: Option<String>,
    },
    Pause {
        id: i64,
    },
    Resume {
        id: i64,
    },
    Retry {
        id: i64,
    },
    Restart {
        id: i64,
    },
    Cancel {
        id: i64,
    },
    SaveStatus {
        id: i64,
    },
    InspectCapture {
        id: u64,
    },
    DownloadCapture {
        id: u64,
        directory: NativePath,
        track: Option<CaptureTrackChoice>,
    },
    DismissCapture {
        id: u64,
    },
    ClearCaptures,
}

/// A client message. `Hello` must be the first message on every connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello { version: u32, token: String },
    Command { command: Command },
}

/// Pairing values shown in a daemon client.
#[derive(Clone, Serialize, Deserialize)]
pub struct CaptureStatus {
    pub endpoint: String,
    pub token: String,
}

impl fmt::Debug for CaptureStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureStatus")
            .field("endpoint", &self.endpoint)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// A media resource reported by the browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureView {
    pub id: u64,
    pub url: String,
    pub kind: String,
    pub content_type: Option<String>,
    pub page_title: Option<String>,
    pub seen: u32,
    pub segmented: bool,
    pub selectable: bool,
}

/// One quality or track offered by a captured presentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureTrackView {
    pub label: String,
    /// Heading that groups qualities sharing the same audio rendition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub details: String,
    pub url: String,
    pub filename: Option<String>,
    pub selection: Option<String>,
}

/// A quality selected for a captured presentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureTrackChoice {
    pub url: String,
    pub filename: Option<String>,
    pub selection: Option<String>,
}

/// The inspected tracks for one captured presentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureTracks {
    pub id: u64,
    pub tracks: Vec<CaptureTrackView>,
    pub problem: Option<String>,
}

/// A job as rendered by a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobView {
    pub id: i64,
    pub name: String,
    pub state: String,
    pub written: u64,
    pub total: Option<u64>,
    pub source: String,
    pub origin: String,
    pub destination: NativePath,
    pub output: Option<NativePath>,
    pub failure_kind: Option<String>,
    pub failure_message: Option<String>,
    pub retryable: bool,
    pub resume_available: bool,
    pub pause_supported: bool,
    pub completion_pending: bool,
}

/// The current daemon state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub jobs: Vec<JobView>,
    pub active: Vec<i64>,
    pub captures: Vec<CaptureView>,
    pub capture_tracks: Option<CaptureTracks>,
    pub capture: Option<CaptureStatus>,
    pub capture_problem: Option<String>,
    pub notice: Option<String>,
}

/// A message from the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMessage {
    Snapshot { snapshot: Snapshot },
    Error { message: String },
}

/// A recovered protocol or transport error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    Io(io::ErrorKind),
    Json,
    InvalidMessage,
    InvalidPath,
    Unauthorized,
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(kind) => write!(f, "IPC transport failed: {kind:?}"),
            Self::Json => f.write_str("IPC message is not valid JSON"),
            Self::InvalidMessage => f.write_str("IPC message is outside the protocol"),
            Self::InvalidPath => f.write_str("IPC path is invalid"),
            Self::Unauthorized => f.write_str("IPC authentication failed"),
        }
    }
}

impl Error for IpcError {}

impl From<io::Error> for IpcError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// Reads one browser native-messaging request.
pub fn read_native_host_request(reader: &mut impl Read) -> Result<NativeHostRequest, IpcError> {
    let mut size = [0_u8; 4];
    reader.read_exact(&mut size)?;
    let size = usize::try_from(u32::from_le_bytes(size)).map_err(|_| IpcError::InvalidMessage)?;
    if size == 0 || size > MAX_NATIVE_HOST_MESSAGE_BYTES {
        return Err(IpcError::InvalidMessage);
    }

    let mut body = vec![0_u8; size];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|_| IpcError::Json)
}

/// Writes one browser native-messaging response.
pub fn write_native_host_response(
    writer: &mut impl Write,
    response: &NativeHostResponse,
) -> Result<(), IpcError> {
    let body = serde_json::to_vec(response).map_err(|_| IpcError::Json)?;
    let size = u32::try_from(body.len()).map_err(|_| IpcError::InvalidMessage)?;
    if body.is_empty() || body.len() > MAX_NATIVE_HOST_MESSAGE_BYTES {
        return Err(IpcError::InvalidMessage);
    }

    writer.write_all(&size.to_le_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Writes a Chrome-compatible native-messaging host manifest.
pub fn write_chrome_native_host_manifest(
    path: &Path,
    executable: &Path,
    extension_id: &str,
) -> Result<(), IpcError> {
    #[derive(Serialize)]
    struct Manifest<'a> {
        name: &'a str,
        description: &'a str,
        path: &'a str,
        #[serde(rename = "type")]
        kind: &'a str,
        allowed_origins: Vec<String>,
    }

    let executable = executable.to_str().ok_or(IpcError::InvalidPath)?;
    let manifest = Manifest {
        name: NATIVE_HOST_NAME,
        description: "Gunda browser capture bridge",
        path: executable,
        kind: "stdio",
        allowed_origins: vec![format!("chrome-extension://{extension_id}/")],
    };
    let body = serde_json::to_vec_pretty(&manifest).map_err(|_| IpcError::Json)?;
    std::fs::write(path, body)?;
    Ok(())
}

/// One side of a newline-delimited JSON connection.
pub struct Connection {
    reader: BufReader<ReadHalf<TcpStream>>,
    writer: WriteHalf<TcpStream>,
}

pub struct Reader {
    inner: BufReader<ReadHalf<TcpStream>>,
}

pub struct Writer {
    inner: WriteHalf<TcpStream>,
}

impl Connection {
    #[must_use]
    pub fn from_stream(stream: TcpStream) -> Self {
        let (reader, writer) = tokio::io::split(stream);

        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }

    #[must_use]
    pub fn into_parts(self) -> (Reader, Writer) {
        (Reader { inner: self.reader }, Writer { inner: self.writer })
    }

    pub async fn send(&mut self, message: &impl Serialize) -> Result<(), IpcError> {
        write_message(&mut self.writer, message).await
    }

    pub async fn receive<T: DeserializeOwned>(&mut self) -> Result<T, IpcError> {
        read_message(&mut self.reader).await
    }
}

impl Reader {
    pub async fn receive<T: DeserializeOwned>(&mut self) -> Result<T, IpcError> {
        read_message(&mut self.inner).await
    }
}

impl Writer {
    pub async fn send(&mut self, message: &impl Serialize) -> Result<(), IpcError> {
        write_message(&mut self.inner, message).await
    }
}

async fn read_message<T: DeserializeOwned>(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> Result<T, IpcError> {
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line).await?;
    if read == 0 {
        return Err(IpcError::Io(io::ErrorKind::UnexpectedEof));
    }
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(IpcError::InvalidMessage);
    }
    serde_json::from_slice(&line).map_err(|_| IpcError::Json)
}

async fn write_message(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    message: &impl Serialize,
) -> Result<(), IpcError> {
    let mut line = serde_json::to_vec(message).map_err(|_| IpcError::Json)?;
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(IpcError::InvalidMessage);
    }
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

/// Connects to a daemon endpoint and sends the protocol greeting.
pub async fn connect(endpoint: &str, token: &str) -> Result<(Connection, Snapshot), IpcError> {
    let stream = TcpStream::connect(endpoint).await?;
    let mut connection = Connection::from_stream(stream);

    connection
        .send(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            token: token.to_owned(),
        })
        .await?;

    let first: ServerMessage = connection.receive().await?;
    match first {
        ServerMessage::Snapshot { snapshot } => Ok((connection, snapshot)),
        ServerMessage::Error { .. } => Err(IpcError::Unauthorized),
    }
}

/// Reads a daemon endpoint from the application data directory.
pub fn read_endpoint(directory: &Path) -> Result<String, IpcError> {
    std::fs::read_to_string(directory.join("gundad.endpoint"))
        .map(|endpoint| endpoint.trim().to_owned())
        .map_err(IpcError::from)
}

/// Stores the daemon endpoint for clients in the application data directory.
pub fn write_endpoint(directory: &Path, endpoint: &str) -> Result<(), IpcError> {
    let path = directory.join("gundad.endpoint");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    use std::io::Write as _;
    let mut file = options.open(path).map_err(IpcError::from)?;
    writeln!(file, "{endpoint}").map_err(IpcError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_native_job_paths() {
        let destination = PathBuf::from("downloads");
        let output = destination.join("file.bin");
        let message = ServerMessage::Snapshot {
            snapshot: Snapshot {
                jobs: vec![JobView {
                    id: 1,
                    name: "file.bin".into(),
                    state: "completed".into(),
                    written: 8,
                    total: Some(8),
                    source: "https://example.test".into(),
                    origin: "Desktop".into(),
                    destination: NativePath::from_path(&destination).unwrap(),
                    output: Some(NativePath::from_path(&output).unwrap()),
                    failure_kind: None,
                    failure_message: None,
                    retryable: false,
                    resume_available: false,
                    pause_supported: false,
                    completion_pending: false,
                }],
                active: Vec::new(),
                notice: None,
                ..Snapshot::default()
            },
        };

        let encoded = serde_json::to_vec(&message).unwrap();
        let ServerMessage::Snapshot { snapshot } =
            serde_json::from_slice::<ServerMessage>(&encoded).unwrap()
        else {
            panic!("a snapshot must round-trip as a snapshot");
        };
        let job = &snapshot.jobs[0];

        assert_eq!(job.destination.to_path().unwrap(), destination);
        assert_eq!(job.output.as_ref().unwrap().to_path().unwrap(), output);
    }

    #[cfg(unix)]
    #[test]
    fn native_path_preserves_non_utf8_bytes_on_unix() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let path = PathBuf::from(OsString::from_vec(vec![b'f', 0x80, b'o']));
        let encoded = NativePath::from_path(&path).unwrap();

        assert_eq!(encoded.to_path().unwrap(), path);
    }

    #[test]
    fn snapshot_round_trips_capture_state() {
        let message = ServerMessage::Snapshot {
            snapshot: Snapshot {
                captures: vec![CaptureView {
                    id: 7,
                    url: "https://cdn.example.test/master.m3u8".into(),
                    kind: "HLS".into(),
                    content_type: Some("application/vnd.apple.mpegurl".into()),
                    page_title: Some("A presentation".into()),
                    seen: 2,
                    segmented: true,
                    selectable: true,
                }],
                capture_tracks: Some(CaptureTracks {
                    id: 7,
                    tracks: vec![CaptureTrackView {
                        label: "640x360".into(),
                        group: Some("English (en)".into()),
                        details: "video | 1.0 MiB".into(),
                        url: "https://cdn.example.test/master.m3u8".into(),
                        filename: Some("presentation.mp4".into()),
                        selection: Some("hls:v1".into()),
                    }],
                    problem: None,
                }),
                capture: Some(CaptureStatus {
                    endpoint: "ws://127.0.0.1:48123/capture".into(),
                    token: "a".repeat(64),
                }),
                ..Snapshot::default()
            },
        };

        let encoded = serde_json::to_vec(&message).unwrap();
        let ServerMessage::Snapshot { snapshot } =
            serde_json::from_slice::<ServerMessage>(&encoded).unwrap()
        else {
            panic!("a snapshot must round-trip as a snapshot");
        };

        assert_eq!(snapshot.captures[0].kind, "HLS");
        assert_eq!(
            snapshot.capture_tracks.unwrap().tracks[0].details,
            "video | 1.0 MiB"
        );
        assert_eq!(
            snapshot.capture.unwrap().endpoint,
            "ws://127.0.0.1:48123/capture"
        );
    }

    #[test]
    fn capture_status_debug_redacts_the_token() {
        let status = CaptureStatus {
            endpoint: "ws://127.0.0.1:48123/capture".into(),
            token: "a".repeat(64),
        };

        assert!(!format!("{status:?}").contains(&status.token));
    }

    #[test]
    fn native_host_request_uses_length_prefixed_json() {
        let body = br#"{"kind":"pairing","version":1,"agent":"test"}"#;
        let mut frame = u32::try_from(body.len()).unwrap().to_le_bytes().to_vec();
        frame.extend_from_slice(body);
        let mut cursor = std::io::Cursor::new(frame);

        assert_eq!(
            read_native_host_request(&mut cursor).unwrap(),
            NativeHostRequest::Pairing {
                version: 1,
                agent: "test".into(),
            }
        );

        let body = br#"{"kind":"choose_directory","version":1}"#;
        let mut frame = u32::try_from(body.len()).unwrap().to_le_bytes().to_vec();
        frame.extend_from_slice(body);
        let mut cursor = std::io::Cursor::new(frame);

        assert_eq!(
            read_native_host_request(&mut cursor).unwrap(),
            NativeHostRequest::ChooseDirectory { version: 1 }
        );
    }

    #[test]
    fn native_host_response_uses_length_prefixed_json() {
        let mut frame = Vec::new();
        write_native_host_response(
            &mut frame,
            &NativeHostResponse::Pairing {
                version: 1,
                endpoint: "127.0.0.1:1234".into(),
                token: "a".repeat(64),
            },
        )
        .unwrap();

        let size = usize::try_from(u32::from_le_bytes(frame[..4].try_into().unwrap())).unwrap();
        assert_eq!(frame.len(), size + 4);
        let value: serde_json::Value = serde_json::from_slice(&frame[4..]).unwrap();
        assert_eq!(value["kind"], "pairing");
        assert_eq!(value["endpoint"], "127.0.0.1:1234");
    }

    #[test]
    fn chrome_native_host_manifest_names_one_extension() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.json");

        write_chrome_native_host_manifest(
            &path,
            Path::new("/opt/gunda/gunda-native-host"),
            "abcdefghijklmnopabcdefghijklmnop",
        )
        .unwrap();

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["name"], NATIVE_HOST_NAME);
        assert_eq!(value["type"], "stdio");
        assert_eq!(
            value["allowed_origins"][0],
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop/"
        );
    }
}
