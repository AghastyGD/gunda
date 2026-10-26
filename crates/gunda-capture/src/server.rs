//! The loopback listener the extension connects to.
//!
//! The listener owns the catalog. A connection never touches it directly: it
//! submits a report over a channel and waits for the identifier, so the catalog
//! is mutated in exactly one place and two connections cannot interleave.
//!
//! Order of business on a connection:
//!
//! 1. the WebSocket upgrade is completed;
//! 2. the request path and origin are checked;
//! 3. the greeting must arrive within [`GREETING_DEADLINE`] and carry the token;
//! 4. only then are reports accepted.
//!
//! The upgrade is answered before the policy is applied, so a refused client
//! sees a completed upgrade and then a closed connection. Nothing is reported,
//! and nothing about the refusal is explained, before step 3 passes.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore, mpsc, oneshot, watch};
use tokio::time::timeout;
use tracing::debug;
use url::Url;

use gunda_core::download::ResourceKind;

use crate::candidate::{Candidate, CandidateId, Catalog, DEFAULT_CAPACITY, Recorded, Report};
use crate::error::CaptureError;
use crate::extractor;
use crate::pairing::PairingToken;
use crate::protocol::{
    ClientMessage, MediaFormat, PROTOCOL_VERSION, Refusal, ServerMessage, encode_server_message,
    parse_client_message,
};
use crate::websocket::WebSocket;

/// The path the extension connects to.
pub const DEFAULT_PATH: &str = "/capture";

/// How long a client has to complete the handshake, or to greet.
const GREETING_DEADLINE: Duration = Duration::from_secs(10);

/// How many connections may be open at once.
const MAX_CONNECTIONS: usize = 8;

/// How many reports may be waiting to be recorded.
const SUBMISSION_CAPACITY: usize = 64;

/// How many instructions may be waiting for the listener.
const COMMAND_CAPACITY: usize = 16;

/// The origin schemes a browser extension uses.
const EXTENSION_SCHEMES: [&str; 3] = [
    "chrome-extension://",
    "moz-extension://",
    "safari-web-extension://",
];

/// An instruction for the running listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureCommand {
    /// Forget every candidate.
    Clear,
    /// Forget one candidate.
    Remove(CandidateId),
}

/// A report on its way to the catalog, with the sender that acknowledges it.
type Submission = (Report, oneshot::Sender<Recorded>);

/// Formats returned by the daemon for one page capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatsResponse {
    pub capture_id: CandidateId,
    pub formats: Vec<MediaFormat>,
}

/// One event delivered to the daemon that owns the capture handle.
pub enum CaptureEvent {
    /// The candidate catalog changed.
    Changed,
    /// The extension requested formats for the newest presentation on one page.
    FormatsRequested {
        candidate: Box<Candidate>,
        reply: oneshot::Sender<Result<FormatsResponse, Refusal>>,
    },
    /// The extension requested one format of the newest presentation on a page.
    DownloadRequested {
        candidate: Box<Candidate>,
        format: Option<String>,
        reply: oneshot::Sender<Result<CandidateId, Refusal>>,
    },
}

enum CaptureRequest {
    Formats {
        page_url: Url,
        reply: oneshot::Sender<Result<FormatsResponse, Refusal>>,
    },
    Download {
        page_url: Url,
        format: Option<String>,
        reply: oneshot::Sender<Result<CandidateId, Refusal>>,
    },
}

/// A snapshot of the capture catalog.
///
/// A snapshot is immutable and cheap to clone: the listener publishes a new one
/// after every change, and a reader never holds a lock on the catalog.
#[derive(Debug, Clone, Default)]
pub struct CatalogView {
    candidates: Arc<[Candidate]>,
}

impl CatalogView {
    fn of(candidates: &[Candidate]) -> Self {
        Self {
            candidates: Arc::from(candidates),
        }
    }

    /// The candidates, newest first.
    #[must_use]
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// Finds a candidate by identifier.
    #[must_use]
    pub fn get(&self, id: CandidateId) -> Option<&Candidate> {
        self.candidates
            .iter()
            .find(|candidate| candidate.id() == id)
    }
}

/// The running capture listener.
pub struct CaptureServer {
    listener: TcpListener,
    token: Arc<PairingToken>,
    catalog: Catalog,
    updates: watch::Sender<CatalogView>,
    commands: mpsc::Receiver<CaptureCommand>,
    capture_requests: mpsc::Receiver<CaptureRequest>,
    request_captures: mpsc::Sender<CaptureRequest>,
    events: mpsc::UnboundedSender<CaptureEvent>,
}

impl CaptureServer {
    /// Binds the listener on an unused loopback port.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Bind`] if no port can be bound.
    pub async fn bind(token: PairingToken) -> Result<(Self, CaptureHandle), CaptureError> {
        Self::bind_to(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), token).await
    }

    /// Binds the listener on a specific address.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Bind`] if the address cannot be bound.
    pub async fn bind_to(
        address: SocketAddr,
        token: PairingToken,
    ) -> Result<(Self, CaptureHandle), CaptureError> {
        let listener = TcpListener::bind(address)
            .await
            .map_err(|error| CaptureError::Bind(error.kind()))?;
        let address = listener
            .local_addr()
            .map_err(|error| CaptureError::Bind(error.kind()))?;

        let (updates, view) = watch::channel(CatalogView::default());
        let (commands, incoming) = mpsc::channel(COMMAND_CAPACITY);
        let (download_commands, download_incoming) = mpsc::channel(COMMAND_CAPACITY);
        let (events, event_events) = mpsc::unbounded_channel();

        let token = Arc::new(token);

        let server = Self {
            listener,
            token: Arc::clone(&token),
            catalog: Catalog::new(DEFAULT_CAPACITY),
            updates,
            commands: incoming,
            capture_requests: download_incoming,
            request_captures: download_commands.clone(),
            events,
        };

        let handle = CaptureHandle {
            address,
            token,
            view,
            commands,
            events: Arc::new(Mutex::new(event_events)),
        };

        Ok((server, handle))
    }

    /// Serves connections until every [`CaptureHandle`] is dropped.
    pub async fn run(mut self) {
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let (submissions, mut reports) = mpsc::channel::<Submission>(SUBMISSION_CAPACITY);

        loop {
            tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                            debug!(%peer, open = MAX_CONNECTIONS, "refused a capture connection");

                            continue;
                        };

                        let token = Arc::clone(&self.token);
                        let submissions = submissions.clone();
                        let download_commands = self.request_captures.clone();

                        tokio::spawn(async move {
                            if let Err(error) =
                                converse(stream, &token, &submissions, &download_commands).await
                            {
                                debug!(%error, "a capture connection ended");
                            }

                            drop(permit);
                        });
                    }
                    Err(error) => debug!(%error, "the capture listener could not accept a connection"),
                },
                Some((report, recorded)) = reports.recv() => {
                    let _ = recorded.send(self.catalog.record(report));
                    self.publish();
                }
                Some(request) = self.capture_requests.recv() => {
                    let page_url = match &request {
                        CaptureRequest::Formats { page_url, .. }
                        | CaptureRequest::Download { page_url, .. } => page_url,
                    };
                    let candidate = self
                        .catalog
                        .candidates()
                        .iter()
                        .filter(|candidate| same_page(candidate.page_url(), page_url))
                        .max_by_key(|candidate| page_candidate_score(candidate))
                        .cloned();
                    match (candidate, request) {
                        (Some(candidate), CaptureRequest::Formats { reply, .. }) => {
                            if let Err(event) =
                                self.events.send(CaptureEvent::FormatsRequested {
                                    candidate: Box::new(candidate),
                                    reply,
                                })
                                && let CaptureEvent::FormatsRequested { reply, .. } = event.0
                            {
                                let _ = reply.send(Err(Refusal::CaptureUnavailable));
                            }
                        }
                        (Some(candidate), CaptureRequest::Download { format, reply, .. }) => {
                            if let Err(event) =
                                self.events.send(CaptureEvent::DownloadRequested {
                                    candidate: Box::new(candidate),
                                    format,
                                    reply,
                                })
                                && let CaptureEvent::DownloadRequested { reply, .. } = event.0
                            {
                                let _ = reply.send(Err(Refusal::CaptureUnavailable));
                            }
                        }
                        (None, CaptureRequest::Formats { reply, .. }) => {
                            let _ = reply.send(Err(Refusal::NoCapture));
                        }
                        (None, CaptureRequest::Download { reply, .. }) => {
                            let _ = reply.send(Err(Refusal::NoCapture));
                        }
                    }
                }
                command = self.commands.recv() => match command {
                    Some(CaptureCommand::Clear) => {
                        self.catalog.clear();
                        self.publish();
                    }
                    Some(CaptureCommand::Remove(id)) => {
                        if self.catalog.remove(id) {
                            self.publish();
                        }
                    }
                    // Every handle is gone, so there is nobody left to serve.
                    None => break,
                },
            }
        }
    }

    /// Publishes the catalog to every reader.
    fn publish(&self) {
        self.updates
            .send_replace(CatalogView::of(self.catalog.candidates()));
    }
}

/// The desktop's end of the capture channel.
#[derive(Debug, Clone)]
pub struct CaptureHandle {
    address: SocketAddr,
    token: Arc<PairingToken>,
    view: watch::Receiver<CatalogView>,
    commands: mpsc::Sender<CaptureCommand>,
    events: Arc<Mutex<mpsc::UnboundedReceiver<CaptureEvent>>>,
}

impl CaptureHandle {
    /// The loopback address the extension connects to.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// The URL the extension connects to.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("ws://{}{DEFAULT_PATH}", self.address)
    }

    /// The token the user pastes into the extension.
    #[must_use]
    pub fn token(&self) -> &PairingToken {
        &self.token
    }

    /// The catalog as it stands.
    #[must_use]
    pub fn view(&self) -> CatalogView {
        self.view.borrow().clone()
    }

    /// Waits until the catalog changes.
    ///
    /// # Errors
    ///
    /// Returns an error once the listener has stopped.
    pub async fn changed(&mut self) -> Result<(), CaptureError> {
        self.view
            .changed()
            .await
            .map_err(|_| CaptureError::Protocol("the capture listener stopped"))
    }

    /// Waits for the next catalog or download event.
    ///
    /// # Errors
    ///
    /// Returns an error once the listener has stopped.
    pub async fn next_event(&mut self) -> Result<CaptureEvent, CaptureError> {
        let events = Arc::clone(&self.events);
        let capture_event = async move {
            let mut events = events.lock().await;
            events.recv().await
        };

        tokio::select! {
            changed = self.view.changed() => {
                changed.map_err(|_| CaptureError::Protocol("the capture listener stopped"))?;
                Ok(CaptureEvent::Changed)
            }
            event = capture_event => event
                .ok_or(CaptureError::Protocol("the capture event channel stopped")),
        }
    }

    /// Forgets every candidate.
    ///
    /// # Errors
    ///
    /// Returns an error once the listener has stopped.
    pub async fn clear(&self) -> Result<(), CaptureError> {
        self.send(CaptureCommand::Clear).await
    }

    /// Forgets one candidate.
    ///
    /// # Errors
    ///
    /// Returns an error once the listener has stopped.
    pub async fn remove(&self, id: CandidateId) -> Result<(), CaptureError> {
        self.send(CaptureCommand::Remove(id)).await
    }

    async fn send(&self, command: CaptureCommand) -> Result<(), CaptureError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| CaptureError::Protocol("the capture listener stopped"))
    }
}

/// Serves one connection: greet, then record reports until it closes.
async fn converse(
    stream: TcpStream,
    token: &PairingToken,
    submissions: &mpsc::Sender<Submission>,
    download_commands: &mpsc::Sender<CaptureRequest>,
) -> Result<(), CaptureError> {
    let mut socket = WebSocket::new(stream);

    let handshake = timeout(GREETING_DEADLINE, socket.accept())
        .await
        .map_err(|_| CaptureError::Protocol("the handshake did not arrive in time"))??;

    if handshake.path() != DEFAULT_PATH {
        return Err(CaptureError::Protocol(
            "the request path is not the capture path",
        ));
    }

    if !origin_allowed(handshake.origin()) {
        return Err(CaptureError::Protocol(
            "the connection did not come from an extension",
        ));
    }

    let greeting = timeout(GREETING_DEADLINE, socket.receive())
        .await
        .map_err(|_| CaptureError::Protocol("the greeting did not arrive in time"))??;
    let greeting = greeting.ok_or(CaptureError::Protocol("the client closed before greeting"))?;

    match parse_client_message(&greeting)? {
        ClientMessage::Hello {
            version,
            token: offered,
            ..
        } => {
            if version > PROTOCOL_VERSION {
                respond(
                    &mut socket,
                    ServerMessage::Refused {
                        reason: Refusal::UnsupportedVersion,
                    },
                )
                .await?;
                socket.close().await?;

                return Ok(());
            }

            if !token.matches(&offered) {
                // Say nothing at all: a client without the token learns nothing
                // from the refusal, not even that the listener exists.
                return Err(CaptureError::Protocol("the pairing token did not match"));
            }

            debug!("a capture client connected");
        }
        ClientMessage::Candidate { .. }
        | ClientMessage::Source { .. }
        | ClientMessage::DownloadPage { .. }
        | ClientMessage::ListFormats { .. } => {
            respond(
                &mut socket,
                ServerMessage::Malformed {
                    reason: String::from("the first message must be a greeting"),
                },
            )
            .await?;
            socket.close().await?;

            return Ok(());
        }
    }

    respond(
        &mut socket,
        ServerMessage::Ready {
            version: PROTOCOL_VERSION,
        },
    )
    .await?;

    loop {
        let Some(text) = socket.receive().await? else {
            return Ok(());
        };

        let response = match parse_client_message(&text) {
            Ok(ClientMessage::Candidate {
                url,
                content_type,
                page_url,
                page_title,
            }) => match Report::new(
                &url,
                content_type.as_deref(),
                &page_url,
                page_title.as_deref(),
            ) {
                Ok(report) => match submit(submissions, report).await? {
                    Recorded {
                        duplicate: true,
                        id,
                        ..
                    } => ServerMessage::Duplicate { id },
                    Recorded { id, .. } => ServerMessage::Accepted { id },
                },
                Err(reason) => ServerMessage::Refused { reason },
            },
            Ok(ClientMessage::Source {
                source_url,
                page_url,
                page_title,
                document,
            }) => match source_reports(&source_url, &page_url, page_title.as_deref(), &document) {
                Ok(reports) => {
                    let mut accepted = Vec::with_capacity(reports.len());
                    let mut duplicates = Vec::with_capacity(reports.len());

                    for report in reports {
                        let recorded = submit(submissions, report).await?;
                        if recorded.duplicate {
                            duplicates.push(recorded.id);
                        } else {
                            accepted.push(recorded.id);
                        }
                    }

                    ServerMessage::Recorded {
                        accepted,
                        duplicates,
                    }
                }
                Err(reason) => ServerMessage::Refused { reason },
            },
            Ok(ClientMessage::DownloadPage { page_url, format }) => {
                let page_url = Url::parse(&page_url).map_err(|_| Refusal::UnsupportedUrl);
                match page_url {
                    Ok(page_url) => {
                        match request_download(download_commands, page_url, format).await? {
                            Ok(id) => ServerMessage::DownloadQueued { id },
                            Err(reason) => ServerMessage::DownloadUnavailable { reason },
                        }
                    }
                    Err(reason) => ServerMessage::DownloadUnavailable { reason },
                }
            }
            Ok(ClientMessage::ListFormats { page_url }) => {
                let page_url = Url::parse(&page_url).map_err(|_| Refusal::UnsupportedUrl);
                match page_url {
                    Ok(page_url) => match request_formats(download_commands, page_url).await? {
                        Ok(formats) => ServerMessage::PageFormats {
                            capture_id: formats.capture_id,
                            formats: formats.formats,
                        },
                        Err(reason) => ServerMessage::FormatsUnavailable { reason },
                    },
                    Err(reason) => ServerMessage::FormatsUnavailable { reason },
                }
            }
            Ok(ClientMessage::Hello { .. }) => ServerMessage::Malformed {
                reason: String::from("the greeting was already sent"),
            },
            Err(CaptureError::Protocol(reason)) => ServerMessage::Malformed {
                reason: reason.to_owned(),
            },
            Err(_) => ServerMessage::Malformed {
                reason: String::from("the message could not be read"),
            },
        };

        respond(&mut socket, response).await?;
    }
}

fn same_page(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
        && left.path() == right.path()
        && page_query_key(left) == page_query_key(right)
}

fn page_query_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if host == "youtube.com" || host.ends_with(".youtube.com") {
        return url
            .query_pairs()
            .find(|(name, _)| name == "v")
            .map_or_else(String::new, |(_, value)| value.into_owned());
    }
    if host == "instagram.com" || host.ends_with(".instagram.com") {
        return String::new();
    }
    if host == "facebook.com" || host.ends_with(".facebook.com") {
        return url
            .query_pairs()
            .find(|(name, _)| name == "v")
            .map_or_else(String::new, |(_, value)| value.into_owned());
    }

    let mut parts = url
        .query_pairs()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            name != "t"
                && name != "si"
                && name != "feature"
                && name != "fbclid"
                && name != "gclid"
                && name != "igsh"
                && name != "igshid"
                && name != "rdid"
                && !name.starts_with("utm_")
        })
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>();
    parts.sort();
    parts.join("&")
}

fn page_candidate_score(candidate: &Candidate) -> (u8, u8, u64) {
    let url = candidate.url().as_str().to_ascii_lowercase();
    let variant_list = u8::from(candidate.variants().len() > 1);
    let media_rank = if url.contains("hls_variant") || url.contains("/master") {
        4
    } else if matches!(candidate.kind(), ResourceKind::Hls | ResourceKind::Dash) {
        3
    } else {
        1
    };

    (variant_list, media_rank, candidate.id())
}

async fn request_download(
    download_commands: &mpsc::Sender<CaptureRequest>,
    page_url: Url,
    format: Option<String>,
) -> Result<Result<CandidateId, Refusal>, CaptureError> {
    let (acknowledge, acknowledged) = oneshot::channel();
    download_commands
        .send(CaptureRequest::Download {
            page_url,
            format,
            reply: acknowledge,
        })
        .await
        .map_err(|_| CaptureError::Protocol("the capture listener is unavailable"))?;
    acknowledged
        .await
        .map_err(|_| CaptureError::Protocol("the capture listener is unavailable"))
}

async fn request_formats(
    download_commands: &mpsc::Sender<CaptureRequest>,
    page_url: Url,
) -> Result<Result<FormatsResponse, Refusal>, CaptureError> {
    let (acknowledge, acknowledged) = oneshot::channel();
    download_commands
        .send(CaptureRequest::Formats {
            page_url,
            reply: acknowledge,
        })
        .await
        .map_err(|_| CaptureError::Protocol("the capture listener is unavailable"))?;
    acknowledged
        .await
        .map_err(|_| CaptureError::Protocol("the capture listener is unavailable"))
}

fn source_reports(
    source_url: &str,
    page_url: &str,
    page_title: Option<&str>,
    document: &serde_json::Value,
) -> Result<Vec<Report>, Refusal> {
    let source_url = Url::parse(source_url).map_err(|_| Refusal::UnsupportedUrl)?;
    let page_url = Url::parse(page_url).map_err(|_| Refusal::UnsupportedUrl)?;

    if page_url.cannot_be_a_base() {
        return Err(Refusal::UnsupportedUrl);
    }

    extractor::extract(&page_url, &source_url, document)?
        .into_iter()
        .map(|presentation| Report::from_extracted(presentation, &page_url, page_title))
        .collect()
}

/// Hands a report to the listener and waits for the identifier.
async fn submit(
    submissions: &mpsc::Sender<Submission>,
    report: Report,
) -> Result<Recorded, CaptureError> {
    let (acknowledge, acknowledged) = oneshot::channel();

    submissions
        .send((report, acknowledge))
        .await
        .map_err(|_| CaptureError::Protocol("the capture catalog is gone"))?;

    acknowledged
        .await
        .map_err(|_| CaptureError::Protocol("the capture catalog is gone"))
}

/// Answers one message.
async fn respond(
    socket: &mut WebSocket<TcpStream>,
    message: ServerMessage,
) -> Result<(), CaptureError> {
    socket.send_text(&encode_server_message(&message)).await?;

    Ok(())
}

/// Whether a connection's origin may report captures.
///
/// A browser extension sends an extension origin; a page sends its own site
/// origin. Rejecting site origins means a page cannot use the listener even if
/// it somehow learned the token. A client that sends no origin at all is a
/// native one -- the tests, or a script -- and is judged by the token alone.
fn origin_allowed(origin: Option<&str>) -> bool {
    match origin {
        Some(origin) => EXTENSION_SCHEMES
            .iter()
            .any(|scheme| origin.starts_with(scheme)),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use tokio::time::{Duration, timeout};
    use url::Url;

    use super::{CaptureHandle, CaptureServer, DEFAULT_PATH};
    use crate::pairing::PairingToken;
    use crate::protocol::{ClientMessage, PROTOCOL_VERSION};
    use crate::test_client::TestClient;

    const EXTENSION_ORIGIN: &str = "chrome-extension://abcdefghijklmnopabcdefghijklmnop";

    async fn listener() -> (CaptureServer, CaptureHandle) {
        CaptureServer::bind(PairingToken::generate().expect("randomness is available"))
            .await
            .expect("the listener binds")
    }

    /// Keeps a broken implementation from hanging the suite.
    async fn within<F: Future>(future: F) -> F::Output {
        timeout(Duration::from_secs(5), future)
            .await
            .expect("the test must not block")
    }

    /// Waits until the catalog satisfies a condition.
    async fn settle(handle: &mut CaptureHandle, condition: impl Fn(&super::CatalogView) -> bool) {
        within(async {
            while !condition(&handle.view()) {
                handle.changed().await.expect("the listener is running");
            }
        })
        .await;
    }

    fn greeting(token: &PairingToken) -> String {
        serde_json::to_string(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            token: token.as_str().to_owned(),
            agent: String::from("gunda-capture-tests"),
        })
        .expect("the greeting encodes")
    }

    fn report(name: &str) -> String {
        format!(
            r#"{{"kind":"candidate","url":"https://cdn.example.com/{name}.mp4","content_type":"video/mp4","page_url":"https://example.com/watch","page_title":"A movie"}}"#
        )
    }

    #[test]
    fn youtube_tracking_parameters_do_not_split_video_pages() {
        let first = Url::parse("https://www.youtube.com/watch?v=video-1&t=20").unwrap();
        let second = Url::parse("https://www.youtube.com/watch?v=video-1&si=tracking").unwrap();
        let other = Url::parse("https://www.youtube.com/watch?v=video-2").unwrap();

        assert!(super::same_page(&first, &second));
        assert!(!super::same_page(&first, &other));
    }

    /// Connects, greets, and returns the live client.
    async fn connected(handle: &CaptureHandle) -> TestClient {
        let (mut client, head) = within(TestClient::connect(
            handle.address(),
            DEFAULT_PATH,
            Some(EXTENSION_ORIGIN),
        ))
        .await;

        assert!(
            head.starts_with("HTTP/1.1 101"),
            "the upgrade must be answered, got: {head}"
        );

        within(client.send(&greeting(handle.token()))).await;

        assert_eq!(
            within(client.receive()).await.as_deref(),
            Some(r#"{"kind":"ready","version":3}"#)
        );

        client
    }

    #[tokio::test]
    async fn records_a_report_and_deduplicates_it() {
        let (server, mut handle) = listener().await;
        tokio::spawn(server.run());

        let mut client = connected(&handle).await;

        within(client.send(&report("movie"))).await;

        assert_eq!(
            within(client.receive()).await.as_deref(),
            Some(r#"{"kind":"accepted","id":0}"#)
        );

        within(client.send(&report("movie"))).await;

        assert_eq!(
            within(client.receive()).await.as_deref(),
            Some(r#"{"kind":"duplicate","id":0}"#)
        );

        settle(&mut handle, |view| view.len() == 1).await;

        let view = handle.view();
        let candidate = &view.candidates()[0];

        assert_eq!(candidate.id(), 0);
        assert_eq!(candidate.seen(), 2);
        assert_eq!(candidate.content_type(), Some("video/mp4"));
        assert_eq!(candidate.path_segment(), Some("movie.mp4"));

        let origin = candidate.origin();

        let gunda_core::download::DownloadOrigin::Browser {
            page_url,
            page_title,
        } = origin
        else {
            panic!("a capture is attributed to the browser");
        };

        assert_eq!(page_url.as_str(), "https://example.com/watch");
        assert_eq!(page_title.as_deref(), Some("A movie"));
    }

    #[tokio::test]
    async fn refuses_a_report_it_cannot_download() {
        let (server, handle) = listener().await;
        tokio::spawn(server.run());

        let mut client = connected(&handle).await;

        within(client.send(
            r#"{"kind":"candidate","url":"https://cdn.example.com/segment-3.ts","content_type":"video/mp2t","page_url":"https://example.com/watch"}"#,
        ))
        .await;

        assert_eq!(
            within(client.receive()).await.as_deref(),
            Some(r#"{"kind":"refused","reason":"segment_stream"}"#)
        );

        assert!(handle.view().is_empty());
    }

    #[tokio::test]
    async fn answers_a_message_that_is_not_a_report() {
        let (server, handle) = listener().await;
        tokio::spawn(server.run());

        let mut client = connected(&handle).await;

        within(client.send("not json at all")).await;

        assert_eq!(
            within(client.receive()).await.as_deref(),
            Some(r#"{"kind":"malformed","reason":"the message was not valid protocol JSON"}"#)
        );

        // The connection survives a bad message.
        within(client.send(&report("movie"))).await;

        assert_eq!(
            within(client.receive()).await.as_deref(),
            Some(r#"{"kind":"accepted","id":0}"#)
        );
    }

    #[tokio::test]
    async fn records_presentations_extracted_from_a_source() {
        let (server, mut handle) = listener().await;
        tokio::spawn(server.run());

        let mut client = connected(&handle).await;
        let source = serde_json::json!({
            "kind": "source",
            "source_url": "https://www.instagram.com/api/graphql",
            "page_url": "https://www.instagram.com/reels/example/",
            "page_title": "Instagram",
            "document": {
                "media": {
                    "pk": "7",
                    "video_versions": [
                        {"url": "https://cdn.example/low.mp4", "width": 640, "height": 360},
                        {"url": "https://cdn.example/high.mp4", "width": 1080, "height": 1920}
                    ]
                }
            }
        });

        within(client.send(&source.to_string())).await;

        assert_eq!(
            within(client.receive()).await.as_deref(),
            Some(r#"{"kind":"recorded","accepted":[0],"duplicates":[]}"#)
        );

        settle(&mut handle, |view| view.len() == 1).await;
        let view = handle.view();
        let candidate = &view.candidates()[0];

        assert_eq!(candidate.url().as_str(), "https://cdn.example/high.mp4");
        assert_eq!(candidate.variants().len(), 2);
    }

    #[tokio::test]
    async fn requests_the_newest_capture_for_a_downloaded_page() {
        let (server, mut handle) = listener().await;
        tokio::spawn(server.run());

        let mut client = connected(&handle).await;
        within(client.send(&report("movie"))).await;
        within(client.receive()).await;

        within(client.send(r#"{"kind":"download_page","page_url":"https://example.com/watch"}"#))
            .await;

        let (candidate, format, reply) = within(async {
            loop {
                if let super::CaptureEvent::DownloadRequested {
                    candidate,
                    format,
                    reply,
                } = handle.next_event().await.expect("the listener runs")
                {
                    break (candidate, format, reply);
                }
            }
        })
        .await;

        assert_eq!(candidate.id(), 0);
        assert_eq!(format, None);
        assert_eq!(
            candidate.url().as_str(),
            "https://cdn.example.com/movie.mp4"
        );
        reply
            .send(Ok(candidate.id()))
            .expect("the connection waits for the result");

        assert_eq!(
            within(client.receive()).await.as_deref(),
            Some(r#"{"kind":"download_queued","id":0}"#)
        );
    }

    #[tokio::test]
    async fn lists_formats_for_the_newest_capture_on_a_page() {
        let (server, mut handle) = listener().await;
        tokio::spawn(server.run());

        let mut client = connected(&handle).await;
        within(client.send(&report("hls_variant/master"))).await;
        within(client.receive()).await;
        within(client.send(&report("hls_playlist/index"))).await;
        within(client.receive()).await;
        within(client.send(r#"{"kind":"list_formats","page_url":"https://example.com/watch"}"#))
            .await;

        let (candidate, reply) = within(async {
            loop {
                if let super::CaptureEvent::FormatsRequested { candidate, reply } =
                    handle.next_event().await.expect("the listener runs")
                {
                    break (candidate, reply);
                }
            }
        })
        .await;
        reply
            .send(Ok(super::FormatsResponse {
                capture_id: candidate.id(),
                formats: vec![crate::protocol::MediaFormat {
                    label: String::from("Original"),
                    details: String::from("video/mp4"),
                    format: String::from("best"),
                }],
            }))
            .expect("the connection waits for formats");

        assert_eq!(
            within(client.receive()).await.as_deref(),
            Some(
                r#"{"kind":"page_formats","capture_id":0,"formats":[{"label":"Original","details":"video/mp4","format":"best"}]}"#
            )
        );
    }

    #[tokio::test]
    async fn refuses_a_page_download_without_a_capture() {
        let (server, handle) = listener().await;
        tokio::spawn(server.run());

        let mut client = connected(&handle).await;
        within(client.send(r#"{"kind":"download_page","page_url":"https://example.com/missing"}"#))
            .await;

        assert_eq!(
            within(client.receive()).await.as_deref(),
            Some(r#"{"kind":"download_unavailable","reason":"no_capture"}"#)
        );
    }

    #[tokio::test]
    async fn requires_the_greeting_first() {
        let (server, handle) = listener().await;
        tokio::spawn(server.run());

        let (mut client, _) = within(TestClient::connect(
            handle.address(),
            DEFAULT_PATH,
            Some(EXTENSION_ORIGIN),
        ))
        .await;

        within(client.send(&report("movie"))).await;

        assert_eq!(
            within(client.receive()).await.as_deref(),
            Some(r#"{"kind":"malformed","reason":"the first message must be a greeting"}"#)
        );

        assert_eq!(
            within(client.receive()).await,
            None,
            "the connection is closed after the refusal"
        );
    }

    #[tokio::test]
    async fn refuses_a_greeting_with_the_wrong_token() {
        let (server, handle) = listener().await;
        tokio::spawn(server.run());

        let (mut client, _) = within(TestClient::connect(
            handle.address(),
            DEFAULT_PATH,
            Some(EXTENSION_ORIGIN),
        ))
        .await;

        let other = PairingToken::generate().expect("randomness is available");
        within(client.send(&greeting(&other))).await;

        assert_eq!(
            within(client.receive()).await,
            None,
            "an unauthorized client is simply closed"
        );

        assert!(handle.view().is_empty());
    }

    #[tokio::test]
    async fn refuses_a_connection_from_a_web_page() {
        let (server, handle) = listener().await;
        tokio::spawn(server.run());

        let (mut client, _) = within(TestClient::connect(
            handle.address(),
            DEFAULT_PATH,
            Some("https://example.com"),
        ))
        .await;

        assert_eq!(within(client.receive()).await, None);

        // Even with the right token, a page origin is not served.
        let (mut client, _) = within(TestClient::connect(
            handle.address(),
            DEFAULT_PATH,
            Some("https://example.com"),
        ))
        .await;
        within(client.send(&greeting(handle.token()))).await;

        assert_eq!(within(client.receive()).await, None);
        assert!(handle.view().is_empty());
    }

    #[tokio::test]
    async fn refuses_another_request_path() {
        let (server, handle) = listener().await;
        tokio::spawn(server.run());

        let (mut client, _) = within(TestClient::connect(
            handle.address(),
            "/somewhere-else",
            None,
        ))
        .await;

        assert_eq!(within(client.receive()).await, None);
    }

    #[tokio::test]
    async fn forgets_candidates_on_request() {
        let (server, mut handle) = listener().await;
        tokio::spawn(server.run());

        let mut client = connected(&handle).await;

        within(client.send(&report("first"))).await;
        within(client.receive()).await;
        within(client.send(&report("second"))).await;
        within(client.receive()).await;

        settle(&mut handle, |view| view.len() == 2).await;

        let second = handle.view().candidates()[0].id();
        let first = handle.view().candidates()[1].id();

        within(handle.remove(first))
            .await
            .expect("the listener runs");
        settle(&mut handle, |view| view.len() == 1).await;

        assert!(handle.view().get(first).is_none());
        assert!(handle.view().get(second).is_some());

        within(handle.clear()).await.expect("the listener runs");
        settle(&mut handle, |view| view.is_empty()).await;

        assert!(handle.view().is_empty());
    }

    #[tokio::test]
    async fn stops_when_the_last_handle_is_dropped() {
        let (server, handle) = listener().await;
        let running = tokio::spawn(server.run());

        drop(handle);

        within(running).await.expect("the listener stops");
    }
}
