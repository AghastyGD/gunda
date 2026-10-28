//! Browser media capture and source extraction for Gunda.
//!
//! A browser extension cannot open a raw socket, so it reports the media it
//! detects to a small loopback service the Gunda daemon runs. The
//! channel has two properties that matter for safety:
//!
//! - it authenticates with a shared secret, so any page in the browser cannot
//!   inject downloads into Gunda;
//! - the wire protocol is documented JSON with explicit limits rather than a
//!   bespoke binary framing.
//!
//! This crate owns the listener, the RFC 6455 subset needed to accept a browser
//! connection, bounded source extractors, the message protocol, the candidate
//! model with deduplication, and pairing-token storage. It does not talk to
//! `gunda-http` or the job manager; the caller decides what a candidate becomes.
//!
//! ```no_run
//! use gunda_capture::{CaptureServer, load_or_create_token, token_path};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let token = load_or_create_token(&token_path(std::path::Path::new(".")))?;
//! let (server, mut handle) = CaptureServer::bind(token).await?;
//!
//! tokio::spawn(server.run());
//!
//! // What the user pastes into the extension.
//! println!("{} with token {}", handle.endpoint(), handle.token().as_str());
//!
//! while handle.changed().await.is_ok() {
//!     for candidate in handle.view().candidates() {
//!         println!("{}", candidate.url());
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! URLs, page titles, and tokens are never logged by this crate, and the token
//! has no printable `Debug`.

mod candidate;
mod error;
mod extractor;
mod pairing;
mod protocol;
mod server;
#[cfg(any(test, feature = "test-client"))]
pub mod test_client;
mod websocket;

pub use candidate::{Candidate, CandidateId, Catalog, DEFAULT_CAPACITY, Recorded, Report};
pub use error::CaptureError;
pub use extractor::{ExtractedPresentation, MediaVariant, extract};
pub use pairing::{
    PairingToken, endpoint_path, load_endpoint, load_or_create_token, load_token, save_endpoint,
    save_token, token_path,
};
pub use protocol::{
    CaptureDownloadStatus, ClientMessage, MediaFormat, PROTOCOL_VERSION, Refusal, ServerMessage,
    encode_server_message, parse_client_message,
};
pub use server::{
    CaptureEvent, CaptureHandle, CaptureServer, CatalogView, DEFAULT_PATH, FormatsResponse,
};
pub use websocket::{MAX_MESSAGE_BYTES, WebSocketError};
