//! Capture failure types.

use std::error::Error;
use std::fmt;
use std::io;

use crate::websocket::WebSocketError;

/// Failure while running the capture channel.
///
/// Nothing here carries a URL, a token, or a page title, so an error can be
/// logged or shown without leaking what the user was browsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureError {
    /// The loopback listener could not be bound.
    Bind(io::ErrorKind),

    /// The pairing token is malformed.
    InvalidToken,

    /// The pairing token file could not be read or written.
    TokenFile(io::ErrorKind),

    /// The operating system refused to provide secure randomness.
    Random,

    /// A connection failed at the transport level.
    Transport(WebSocketError),

    /// A message was not valid protocol data.
    Protocol(&'static str),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(kind) => write!(f, "the capture listener could not be bound ({kind:?})"),
            Self::InvalidToken => f.write_str("the capture pairing token is not valid"),
            Self::TokenFile(kind) => {
                write!(f, "the capture pairing token file is unusable ({kind:?})")
            }
            Self::Random => f.write_str("secure randomness is unavailable"),
            Self::Transport(error) => write!(f, "capture connection failed: {error}"),
            Self::Protocol(reason) => write!(f, "capture protocol violation: {reason}"),
        }
    }
}

impl Error for CaptureError {}

impl From<WebSocketError> for CaptureError {
    fn from(error: WebSocketError) -> Self {
        Self::Transport(error)
    }
}
