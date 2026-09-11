use std::error::Error;
use std::fmt;

/// HTTP failures withour request URLs, headers, or response bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpError {
    Configuration,
    InvalidRequest,
    Timeout,
    Transport,
    UnexpectedStatus(u16),
    InvalidMetadata,
    UnsupportedEncoding,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration => f.write_str("could not configure the HTTP client"),
            Self::InvalidRequest => f.write_str("HTTP request context is invalid or unsupported"),
            Self::Timeout => f.write_str("HTTP request timed out"),
            Self::Transport => f.write_str("HTTP transport failed"),
            Self::UnexpectedStatus(status) => {
                write!(f, "unexpected HTTP response status: {status}")
            }
            Self::InvalidMetadata => f.write_str("HTTP response metadata is invalid"),
            Self::UnsupportedEncoding => f.write_str("HTTP content encoding is not supported"),
        }
    }
}

impl Error for HttpError {}

// TODO: improve error classification without exposing request context
pub(crate) fn map_request_error(error: reqwest::Error) -> HttpError {
    if error.is_timeout() {
        HttpError::Timeout
    } else {
        HttpError::Transport
    }
}
