mod common;

use std::path::Path;

use common::serve_once;
use gunda_core::application::{DownloadExecutor, ExecutionInput, PreparedTransfer, StagedTransfer};
use gunda_core::download::{
    DownloadDestination, DownloadId, FailureKind, FileConflictPolicy, RequestContext, ResourceKind,
};
use gunda_http::{HttpClient, HttpExecutor, partial_path};
use tempfile::tempdir;
use url::Url;

fn test_id() -> DownloadId {
    DownloadId::new(56).expect("ID must be valid")
}

fn input(url: Url, directory: &Path) -> ExecutionInput {
    ExecutionInput {
        id: test_id(),
        request: RequestContext::new(url, Vec::new()),
        destination: DownloadDestination::new(
            directory.to_path_buf(),
            Some("file.bin".to_owned()),
            FileConflictPolicy::Fail,
        ),
    }
}

#[tokio::test]
async fn execution_reuses_the_opened_get_across_phases() {
    let directory = tempdir().expect("temporary directory must exist");

    let (url, server) = serve_once(
        "HTTP/1.1 200 OK\r\n\
         Content-Length: 5\r\n\
         Content-Type: application/octet-stream\r\n\
         Connection: close\r\n\
         \r\n\
         hello",
    )
    .await;

    let executor = HttpExecutor::new(HttpClient::new().expect("client must build"));

    let staging = partial_path(directory.path(), test_id());
    let final_path = directory.path().join("file.bin");

    let prepared = executor
        .prepare(input(url, directory.path()))
        .await
        .expect("preparation must succeed");

    assert_eq!(prepared.total_bytes(), Some(5));

    let resource = prepared.resource();

    assert_eq!(resource.kind(), ResourceKind::File);
    assert_eq!(resource.content_type(), Some("application/octet-stream"));
    assert!(!staging.exists());
    assert!(!final_path.exists());

    // The one-request server has finished before transfer begins.
    // A second GET would no longer have a listening server.
    let received = server.await.expect("server task must succeed");
    assert!(received.starts_with("GET /file.bin HTTP/1.1\r\n"));

    let staged = prepared
        .transfer()
        .await
        .expect("transfer must use the existing response");

    assert_eq!(staged.written_bytes(), 5);
    assert!(staging.exists());
    assert!(!final_path.exists());

    let output = staged.finalize().await.expect("finalization must succeed");

    assert_eq!(output.destination.final_path(), final_path.as_path());
    assert_eq!(output.written_bytes, 5);
    assert!(!output.cleanup_pending);
    assert!(!staging.exists());

    assert_eq!(
        tokio::fs::read(&final_path)
            .await
            .expect("final output must be readable"),
        b"hello",
    );
}

#[tokio::test]
async fn preparation_failure_creates_no_partial_file() {
    let directory = tempdir().expect("temporary directory must exist");

    let (url, server) = serve_once(
        "HTTP/1.1 403 Forbidden\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n",
    )
    .await;

    let executor = HttpExecutor::new(HttpClient::new().expect("client must build"));

    let failure = match executor.prepare(input(url, directory.path())).await {
        Ok(_) => panic!("authorization rejection must fail preparation"),
        Err(failure) => failure,
    };

    assert_eq!(failure.kind(), FailureKind::Authentication);
    assert!(!failure.is_retryable());
    assert!(!partial_path(directory.path(), test_id()).exists());

    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn finalization_conflict_preserves_both_files() {
    let directory = tempdir().expect("temporary directory must exist");
    let final_path = directory.path().join("file.bin");

    tokio::fs::write(&final_path, b"existing")
        .await
        .expect("existing output must be created");

    let (url, server) = serve_once(
        "HTTP/1.1 200 OK\r\n\
         Content-Length: 3\r\n\
         Connection: close\r\n\
         \r\n\
         new",
    )
    .await;

    let executor = HttpExecutor::new(HttpClient::new().expect("client must build"));

    let prepared = executor
        .prepare(input(url, directory.path()))
        .await
        .expect("preparation must succeed");

    let staged = prepared.transfer().await.expect("transfer must succeed");

    let failure = match staged.finalize().await {
        Ok(_) => panic!("existing destination must prevent publication"),
        Err(failure) => failure,
    };

    assert_eq!(failure.kind(), FailureKind::Storage);

    assert_eq!(
        tokio::fs::read(&final_path)
            .await
            .expect("existing output must remain readable"),
        b"existing",
    );

    assert_eq!(
        tokio::fs::read(partial_path(directory.path(), test_id()))
            .await
            .expect("partial output must remain readable"),
        b"new",
    );

    server.await.expect("server task must succeed");
}
