mod common;

use std::io::ErrorKind;

use common::serve_once;
use gunda_core::download::{DownloadId, RequestContext};
use gunda_http::{
    FileOperation, HttpClient, HttpError, PartialDownloadError, download_to_partial, partial_path,
};
use tempfile::tempdir;

fn test_id() -> DownloadId {
    DownloadId::new(13).expect("test ID must be valid")
}

#[tokio::test]
async fn successful_bodies_are_written_to_partial_files() {
    let cases: [(&str, &[u8], Option<u64>); 3] = [
        (
            "HTTP/1.1 200 OK\r\n\
             Content-Length: 11\r\n\
             Connection: close\r\n\
             \r\n\
             hello world",
            b"hello world",
            Some(11),
        ),
        (
            "HTTP/1.1 200 OK\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\
             \r\n",
            b"",
            Some(0),
        ),
        (
            "HTTP/1.1 200 OK\r\n\
             Transfer-Encoding: chunked\r\n\
             Connection: close\r\n\
             \r\n\
             5\r\nhello\r\n\
             0\r\n\r\n",
            b"hello",
            None,
        ),
    ];

    for (response, expected, expected_length) in cases {
        let directory = tempdir().expect("temporary directory must exist");
        let (url, server) = serve_once(response).await;

        let client = HttpClient::new().expect("client must build");
        let request = RequestContext::new(url, Vec::new());

        let partial = download_to_partial(&client, &request, test_id(), directory.path())
            .await
            .expect("transfer must succeed");

        let expected_path = partial_path(directory.path(), test_id());

        assert_eq!(partial.path(), expected_path.as_path());
        assert_eq!(
            partial.written_bytes(),
            u64::try_from(expected.len()).expect("test size must fit"),
        );
        assert_eq!(partial.metadata().content_length(), expected_length);

        let contents = tokio::fs::read(partial.path())
            .await
            .expect("partial file must be readable");

        assert_eq!(contents.as_slice(), expected);

        // No output has been promoted to a final destination.
        assert!(!directory.path().join("file.bin").exists());

        server.await.expect("server task must succeed");
    }
}

#[tokio::test]
async fn existing_partial_file_is_never_overwritten() {
    let directory = tempdir().expect("temporary directory must exist");
    let path = partial_path(directory.path(), test_id());

    tokio::fs::write(&path, b"existing data")
        .await
        .expect("existing file must be created");

    let (url, server) = serve_once(
        "HTTP/1.1 200 OK\r\n\
         Content-Length: 3\r\n\
         Connection: close\r\n\
         \r\n\
         new",
    )
    .await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());

    let result = download_to_partial(&client, &request, test_id(), directory.path()).await;

    assert!(matches!(
        result,
        Err(PartialDownloadError::File {
            operation: FileOperation::Create,
            kind: ErrorKind::AlreadyExists,
        })
    ));

    let contents = tokio::fs::read(&path)
        .await
        .expect("existing file must remain readable");

    assert_eq!(contents.as_slice(), b"existing data");

    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn interrupted_transfer_preserves_partial_without_reporting_success() {
    let directory = tempdir().expect("temporary directory must exist");

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

    let result = download_to_partial(&client, &request, test_id(), directory.path()).await;

    assert!(matches!(
        result,
        Err(PartialDownloadError::Http(
            HttpError::Transport | HttpError::BodyLengthMismatch
        ))
    ));

    let path = partial_path(directory.path(), test_id());

    assert!(path.exists());
    assert!(!directory.path().join("file.bin").exists());

    let length = tokio::fs::metadata(&path)
        .await
        .expect("partial file must remain")
        .len();

    // The HTTP parser may report failure before delivering any bytes.
    assert!(length <= 5);

    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn rejected_http_response_does_not_create_a_partial_file() {
    let directory = tempdir().expect("temporary directory must exist");

    let (url, server) = serve_once(
        "HTTP/1.1 404 Not Found\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n",
    )
    .await;

    let client = HttpClient::new().expect("client must build");
    let request = RequestContext::new(url, Vec::new());

    let result = download_to_partial(&client, &request, test_id(), directory.path()).await;

    assert!(matches!(
        result,
        Err(PartialDownloadError::Http(HttpError::UnexpectedStatus(404)))
    ));

    assert!(!partial_path(directory.path(), test_id()).exists());

    server.await.expect("server task must succeed");
}
