//! The capture wire protocol.
//!
//! Messages are JSON objects with a `kind` field. The extension is free to send
//! fields this build does not know: unknown fields are ignored on purpose, so a
//! newer extension keeps working with an older Gunda. An unknown `kind`, on the
//! other hand, is refused rather than guessed at.
//!
//! The protocol carries no cookies and no request headers. The extension
//! reports *what* the browser asked for; replaying the browser's credentials
//! would move them into Gunda's process, its logs, and its job database. A
//! download that needs a session cookie is out of scope until there is a
//! deliberate place to keep one.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::CaptureError;

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 2;

/// Largest message accepted, in bytes.
pub use crate::websocket::MAX_MESSAGE_BYTES;

/// Why a report was not turned into a candidate.
///
/// The reasons are stable strings so the extension can show its own message
/// for each one: the desktop and the extension are versioned separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// The media URL is not an absolute `http` or `https` URL.
    UnsupportedUrl,
    /// The resource is not something this build can download.
    UnsupportedMedia,
    /// The report is one segment of a segmented stream, not the stream itself.
    SegmentStream,
    /// No extractor is available for the page that supplied the source.
    UnsupportedSource,
    /// The extension speaks a protocol this build does not.
    UnsupportedVersion,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::UnsupportedUrl => "the media URL is not an absolute HTTP URL",
            Self::UnsupportedMedia => "the resource is not downloadable media",
            Self::SegmentStream => "the report is a stream segment, not a stream",
            Self::UnsupportedSource => "the page source is not supported",
            Self::UnsupportedVersion => "the extension speaks a newer protocol",
        };

        f.write_str(reason)
    }
}

/// A message from the extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientMessage {
    /// The first message on every connection.
    Hello {
        /// The protocol version the extension speaks.
        version: u32,
        /// The pairing token the user pasted into the extension.
        token: String,
        /// The extension build, for diagnostics.
        agent: String,
    },

    /// A media resource the extension detected.
    Candidate {
        /// The media URL, absolute and `http` or `https`.
        url: String,
        /// The response content type, when the extension saw one.
        #[serde(default)]
        content_type: Option<String>,
        /// The page the resource was found on.
        page_url: String,
        /// The page title, when the extension has one.
        #[serde(default)]
        page_title: Option<String>,
    },

    /// A bounded page response for daemon-side extraction.
    Source {
        /// The response URL, used to attribute extraction failures.
        source_url: String,
        /// The page the response belongs to.
        page_url: String,
        /// The page title, when the extension has one.
        #[serde(default)]
        page_title: Option<String>,
        /// Parsed JSON, already bounded by the message limit.
        document: Value,
    },
}

/// A message to the extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The greeting was accepted and the channel is live.
    Ready {
        /// The protocol version the desktop speaks.
        version: u32,
    },
    /// The report is now a candidate.
    Accepted {
        /// The candidate's identifier.
        id: u64,
    },
    /// The report matched a candidate that was already recorded.
    Duplicate {
        /// The candidate's identifier.
        id: u64,
    },
    /// Extracted presentations stored from one source document.
    Recorded {
        accepted: Vec<u64>,
        duplicates: Vec<u64>,
    },
    /// The report was refused, with a stable reason.
    Refused {
        /// Why the report was refused.
        reason: Refusal,
    },
    /// The message could not be read at all.
    Malformed {
        /// A description of the problem, always one of this crate's strings.
        reason: String,
    },
}

/// Parses one message from the extension.
///
/// The size limit is enforced here as well as in the transport so that a caller
/// driving this function directly cannot be made to parse an unbounded string.
pub fn parse_client_message(text: &str) -> Result<ClientMessage, CaptureError> {
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(CaptureError::Protocol("the message was too large"));
    }

    serde_json::from_str(text)
        .map_err(|_| CaptureError::Protocol("the message was not valid protocol JSON"))
}

/// Encodes one message for the extension.
///
/// Encoding this crate's own types cannot fail; the fallback keeps the
/// signature total rather than panicking inside a connection task.
#[must_use]
pub fn encode_server_message(message: &ServerMessage) -> String {
    serde_json::to_string(message).unwrap_or_else(|_| {
        String::from(r#"{"kind":"malformed","reason":"the message could not be encoded"}"#)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ClientMessage, MAX_MESSAGE_BYTES, Refusal, ServerMessage, encode_server_message,
        parse_client_message,
    };

    #[test]
    fn parses_a_greeting() {
        let message = parse_client_message(
            r#"{"kind":"hello","version":2,"token":"aabb","agent":"gunda-extension"}"#,
        )
        .expect("the greeting is valid");

        assert_eq!(
            message,
            ClientMessage::Hello {
                version: 2,
                token: "aabb".to_owned(),
                agent: "gunda-extension".to_owned(),
            }
        );
    }

    #[test]
    fn parses_a_candidate_without_optional_fields() {
        let message = parse_client_message(
            r#"{"kind":"candidate","url":"https://cdn.example/x.m3u8","page_url":"https://example/watch"}"#,
        )
        .expect("the report is valid");

        assert_eq!(
            message,
            ClientMessage::Candidate {
                url: "https://cdn.example/x.m3u8".to_owned(),
                content_type: None,
                page_url: "https://example/watch".to_owned(),
                page_title: None,
            }
        );
    }

    #[test]
    fn ignores_fields_a_newer_extension_adds() {
        let message = parse_client_message(
            r#"{"kind":"candidate","url":"https://cdn.example/x.mp4","page_url":"https://example/watch","duration_ms":120000}"#,
        )
        .expect("o unknown fields must not break an older build");

        let ClientMessage::Candidate { url, .. } = message else {
            panic!("the message is a report");
        };

        assert_eq!(url, "https://cdn.example/x.mp4");
    }

    #[test]
    fn parses_a_bounded_source_document() {
        let message = parse_client_message(
            r#"{"kind":"source","source_url":"https://example.com/api","page_url":"https://example.com/watch","document":{"media":{"video_url":"https://cdn.example/movie.mp4"}}}"#,
        )
        .expect("the source is valid");

        let ClientMessage::Source {
            source_url,
            document,
            ..
        } = message
        else {
            panic!("the message is a source");
        };

        assert_eq!(source_url, "https://example.com/api");
        assert_eq!(
            document["media"]["video_url"],
            "https://cdn.example/movie.mp4"
        );
    }

    #[test]
    fn refuses_an_unknown_kind() {
        let error = parse_client_message(r#"{"kind":"teleport"}"#).expect_err("unknown kinds fail");

        assert!(error.to_string().contains("protocol"));
    }

    #[test]
    fn refuses_text_that_is_not_json() {
        assert!(parse_client_message("hello there").is_err());
        assert!(parse_client_message("").is_err());
    }

    #[test]
    fn refuses_a_message_over_the_size_limit() {
        let oversized = format!(
            r#"{{"kind":"hello","version":1,"token":"{}","agent":"a"}}"#,
            "0".repeat(MAX_MESSAGE_BYTES)
        );

        assert!(parse_client_message(&oversized).is_err());
    }

    #[test]
    fn encodes_server_messages_as_stable_json() {
        assert_eq!(
            encode_server_message(&ServerMessage::Ready { version: 2 }),
            r#"{"kind":"ready","version":2}"#
        );
        assert_eq!(
            encode_server_message(&ServerMessage::Accepted { id: 3 }),
            r#"{"kind":"accepted","id":3}"#
        );
        assert_eq!(
            encode_server_message(&ServerMessage::Duplicate { id: 3 }),
            r#"{"kind":"duplicate","id":3}"#
        );
        assert_eq!(
            encode_server_message(&ServerMessage::Refused {
                reason: Refusal::SegmentStream
            }),
            r#"{"kind":"refused","reason":"segment_stream"}"#
        );
        assert_eq!(
            encode_server_message(&ServerMessage::Recorded {
                accepted: vec![4],
                duplicates: vec![2],
            }),
            r#"{"kind":"recorded","accepted":[4],"duplicates":[2]}"#
        );
    }
}
