mod common;

use common::serve_once;
use gunda_core::download::RequestContext;
use gunda_http::{HttpClient, HttpError, StrongEntityTag};

#[tokio::test]
async fn range_request_preserves_the_validator_and_streams_remaining_bytes() {
    let (url, server) = serve_once(
        "HTTP/1.1 206 Partial Content\r\n\
         Content-Range: bytes 5-9/10\r\n\
         Content-Length: 5\r\n\
         ETag: \"version-73\"\r\n\
         Connection: close\r\n\
         \r\n\
         world",
    )
    .await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());
    let validator = StrongEntityTag::parse(b"\"version-73\"").expect("validator must be valid");

    let mut body = client
        .open_range(&request, 5, 10, &validator)
        .await
        .expect("range must open");

    assert_eq!(body.range().start(), 5);
    assert_eq!(body.range().end(), 9);
    assert_eq!(body.range().total(), 10);
    assert_eq!(body.metadata().content_length(), Some(5));
    assert_eq!(body.received_bytes(), 0);

    let mut contents = Vec::new();

    while let Some(chunk) = body.next_chunk().await.expect("body must be readable") {
        contents.extend_from_slice(&chunk);
    }

    assert_eq!(contents, b"world");
    assert_eq!(body.received_bytes(), 5);
    assert!(body.is_finished());

    let received = server.await.expect("server task must succeed");
    let received = received.to_ascii_lowercase();

    assert!(received.starts_with("get /file.bin http/1.1\r\n"));
    assert!(received.contains("range: bytes=5-\r\n"));
    assert!(received.contains("if-range: \"version-73\"\r\n"));
    assert!(received.contains("accept-encoding: identity\r\n"));
}

#[tokio::test]
async fn full_and_unsatisfied_responses_are_rejected() {
    for (response, status) in [
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\
             Connection: close\r\n\r\nhello",
            200,
        ),
        (
            "HTTP/1.1 416 Range Not Satisfiable\r\n\
             Content-Range: bytes */10\r\nContent-Length: 0\r\n\
             Connection: close\r\n\r\n",
            416,
        ),
    ] {
        let (url, server) = serve_once(response).await;

        let client = HttpClient::new().expect("client must build");
        let request = RequestContext::new(url, Vec::new());
        let validator = StrongEntityTag::parse(b"\"version-73\"").expect("validator must be valid");

        assert!(matches!(
            client.open_range(&request, 5, 10, &validator).await,
            Err(HttpError::UnexpectedStatus(actual)) if actual == status
        ));

        server.await.expect("server task must succeed");
    }
}

#[tokio::test]
async fn inconsistent_partial_metadata_is_rejected() {
    for headers in [
        "Content-Length: 5\r\n",
        "Content-Range: bytes 4-8/10\r\nContent-Length: 5\r\n",
        "Content-Range: bytes 5-9/11\r\nContent-Length: 5\r\n",
        "Content-Range: bytes 5-8/10\r\nContent-Length: 4\r\n",
        "Content-Range: bytes 5-9/10\r\nContent-Length: 4\r\n",
        "Content-Range: bytes 5-9/*\r\nContent-Length: 5\r\n",
        "Content-Range: bytes 5-9/10\r\n\
         Content-Range: bytes 5-9/10\r\nContent-Length: 5\r\n",
        "Content-Range: bytes 5-9/10\r\nContent-Length: 5\r\n\
         ETag: \"different-version\"\r\n",
        "Content-Range: bytes 5-9/10\r\nContent-Length: 5\r\n\
         ETag: W/\"version-73\"\r\n",
        "Content-Range: bytes 5-9/10\r\nContent-Length: 5\r\n\
         Content-Type: multipart/byteranges; boundary=test\r\n",
    ] {
        let response = format!(
            "HTTP/1.1 206 Partial Content\r\n\
             {headers}Connection: close\r\n\r\nhello"
        );

        let (url, server) = serve_once(&response).await;

        let client = HttpClient::new().expect("client must build");
        let request = RequestContext::new(url, Vec::new());
        let validator = StrongEntityTag::parse(b"\"version-73\"").expect("validator must be valid");

        assert!(matches!(
            client.open_range(&request, 5, 10, &validator).await,
            Err(HttpError::InvalidMetadata)
        ));

        server.await.expect("server task must succeed");
    }
}

#[tokio::test]
async fn chunked_range_uses_the_length_from_content_range() {
    let (url, server) = serve_once(
        "HTTP/1.1 206 Partial Content\r\n\
         Content-Range: bytes 5-9/10\r\n\
         Transfer-Encoding: chunked\r\n\
         Connection: close\r\n\
         \r\n\
         5\r\nworld\r\n\
         0\r\n\r\n",
    )
    .await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());
    let validator = StrongEntityTag::parse(b"\"version-73\"").expect("validator must be valid");

    let mut body = client
        .open_range(&request, 5, 10, &validator)
        .await
        .expect("range must open");

    assert_eq!(body.metadata().content_length(), Some(5));

    let mut contents = Vec::new();

    while let Some(chunk) = body.next_chunk().await.expect("body must be readable") {
        contents.extend_from_slice(&chunk);
    }

    assert_eq!(contents, b"world");
    assert!(body.is_finished());

    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn truncated_range_does_not_finish_successfully() {
    let (url, server) = serve_once(
        "HTTP/1.1 206 Partial Content\r\n\
         Content-Range: bytes 5-9/10\r\n\
         Content-Length: 5\r\n\
         Connection: close\r\n\
         \r\n\
         hi",
    )
    .await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());
    let validator = StrongEntityTag::parse(b"\"version-73\"").expect("validator must be valid");

    let mut body = client
        .open_range(&request, 5, 10, &validator)
        .await
        .expect("headers must be valid");

    let error = loop {
        match body.next_chunk().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("truncated range must not finish successfully"),
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
