use std::time::Duration;

use gunda_core::download::{HeaderSensitivity, RequestContext, RequestHeader};
use gunda_http::{HttpClient, HttpError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use url::Url;

async fn serve_once(response: &'static str) -> (Url, JoinHandle<String>) {
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

#[tokio::test]
async fn inspection_reads_metadata_without_requiring_a_body() {
    let (url, server) = serve_once(
        "HTTP/1.1 200 OK\r\n\
         Content-Length: 1024\r\n\
         Content-Type: application/octet-stream\r\n\
         Connection: close\r\n\
         \r\n",
    )
    .await;

    let request = RequestContext::new(
        url,
        vec![RequestHeader::new(
            "Authorization",
            "Bearer local-test-token",
            HeaderSensitivity::Sensitive,
        )],
    );

    let client = HttpClient::new().expect("client must build");
    let inspection = client
        .inspect(&request)
        .await
        .expect("inspection must succeed");

    assert_eq!(inspection.content_length(), Some(1024));
    assert_eq!(inspection.content_type(), Some("application/octet-stream"),);

    let received = server.await.expect("server task must succeed");
    let received = received.to_ascii_lowercase();

    assert!(received.starts_with("head /file.bin http/1.1\r\n"));
    assert!(received.contains("accept-encoding: identity\r\n"));
    assert!(received.contains("authorization: bearer local-test-token\r\n"));
}

#[tokio::test]
async fn missing_length_is_not_interpreted_as_an_empty_file() {
    let (url, server) = serve_once("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n").await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());

    let inspection = client
        .inspect(&request)
        .await
        .expect("inspection must succeed");

    assert_eq!(inspection.content_length(), None);
    assert_eq!(inspection.content_type(), None);

    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn zero_length_is_preserved() {
    let (url, server) =
        serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());

    let inspection = client
        .inspect(&request)
        .await
        .expect("inspection must succeed");

    assert_eq!(inspection.content_length(), Some(0));

    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn redirects_are_reported_without_following_them() {
    let (url, server) = serve_once(
        "HTTP/1.1 302 Found\r\n\
         Location: /another-file.bin\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n",
    )
    .await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());

    assert!(matches!(
        client.inspect(&request).await,
        Err(HttpError::UnexpectedStatus(302))
    ));

    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn encoded_responses_are_not_treated_as_identity_content() {
    let (url, server) = serve_once(
        "HTTP/1.1 200 OK\r\n\
         Content-Encoding: gzip\r\n\
         Content-Length: 20\r\n\
         Connection: close\r\n\
         \r\n",
    )
    .await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());

    assert!(matches!(
        client.inspect(&request).await,
        Err(HttpError::UnsupportedEncoding)
    ));

    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn invalid_headers_are_rejected_without_exposing_their_values() {
    let request = RequestContext::new(
        Url::parse("http://127.0.0.1:1/private-url-marker").expect("URL must be valid"),
        vec![RequestHeader::new(
            "Authorization",
            "private-header-marker\r\nInjected: value",
            HeaderSensitivity::Sensitive,
        )],
    );

    let client = HttpClient::new().expect("client must build");

    let error = match client.inspect(&request).await {
        Ok(_) => panic!("invalid header must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error, HttpError::InvalidRequest);

    let output = format!("{error} {error:?}");

    assert!(!output.contains("private-url-marker"));
    assert!(!output.contains("private-header-marker"));
}

#[tokio::test]
async fn get_reads_body_and_uses_its_own_metadata() {
    let (url, server) = serve_once(
        "HTTP/1.1 200 OK\r\n\
         Content-Length: 11\r\n\
         Content-Type: application/octet-stream\r\n\
         Connection: close\r\n\
         \r\n\
         hello world",
    )
    .await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());

    let mut body = client.open(&request).await.expect("GET must open");

    assert_eq!(body.metadata().content_length(), Some(11));
    assert_eq!(body.received_bytes(), 0);
    assert!(!body.is_finished());

    let mut received = Vec::new();

    while let Some(chunk) = body.next_chunk().await.expect("read must succeed") {
        received.extend_from_slice(&chunk);
    }

    assert_eq!(received.as_slice(), b"hello world");
    assert_eq!(body.received_bytes(), 11);
    assert!(body.is_finished());
    assert!(
        body.next_chunk()
            .await
            .expect("EOF must remain stable")
            .is_none()
    );

    let request = server.await.expect("server task must succeed");

    assert!(request.starts_with("GET /file.bin HTTP/1.1\r\n"));
}

#[tokio::test]
async fn empty_get_finishes_successfully() {
    let (url, server) =
        serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());

    let mut body = client.open(&request).await.expect("GET must open");

    assert_eq!(body.metadata().content_length(), Some(0));
    assert!(
        body.next_chunk()
            .await
            .expect("read must succeed")
            .is_none()
    );
    assert_eq!(body.received_bytes(), 0);
    assert!(body.is_finished());

    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn chunked_body_finishes_without_a_known_total() {
    let (url, server) = serve_once(
        "HTTP/1.1 200 OK\r\n\
         Transfer-Encoding: chunked\r\n\
         Connection: close\r\n\
         \r\n\
         5\r\nhello\r\n\
         6\r\n world\r\n\
         0\r\n\r\n",
    )
    .await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());

    let mut body = client.open(&request).await.expect("GET must open");

    assert_eq!(body.metadata().content_length(), None);

    let mut received = Vec::new();

    while let Some(chunk) = body.next_chunk().await.expect("read must succeed") {
        received.extend_from_slice(&chunk);
    }

    assert_eq!(received.as_slice(), b"hello world");
    assert_eq!(body.received_bytes(), 11);
    assert!(body.is_finished());

    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn truncated_body_never_becomes_successful_eof() {
    let (url, server) = serve_once(
        "HTTP/1.1 200 OK\r\n\
         Content-Length: 20\r\n\
         Connection: close\r\n\
         \r\n\
         short",
    )
    .await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());

    let mut body = client.open(&request).await.expect("GET must open");

    let error = loop {
        match body.next_chunk().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("truncated body must not finish successfully"),
            Err(error) => break error,
        }
    };

    assert!(matches!(
        error,
        HttpError::Transport | HttpError::BodyLengthMismatch
    ));

    assert!(!body.is_finished());
    assert_eq!(body.next_chunk().await, Err(error));

    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn unsolicited_partial_response_is_rejected() {
    let (url, server) = serve_once(
        "HTTP/1.1 206 Partial Content\r\n\
         Content-Range: bytes 0-4/20\r\n\
         Content-Length: 5\r\n\
         Connection: close\r\n\
         \r\n\
         hello",
    )
    .await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());

    assert!(matches!(
        client.open(&request).await,
        Err(HttpError::UnexpectedStatus(206))
    ));

    server.await.expect("server task must succeed");
}
