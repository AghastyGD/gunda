//! Versioned local IPC for the Gunda daemon and its clients.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::TcpStream;

/// Version of the daemon protocol.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest single protocol message accepted.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

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
}

/// A client message. `Hello` must be the first message on every connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello { version: u32, token: String },
    Command { command: Command },
}

/// A job as rendered by a client.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobView {
    pub id: i64,
    pub name: String,
    pub state: String,
    pub written: u64,
    pub total: Option<u64>,
    pub source: String,
    pub origin: String,
    pub destination: String,
    pub output: Option<String>,
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
