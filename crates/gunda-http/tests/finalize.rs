mod common;

use std::io::ErrorKind;
use std::path::Path;

use common::serve_once;
use gunda_core::application::TransferOutcome;
use gunda_core::download::{DownloadDestination, DownloadId, FileConflictPolicy, RequestContext};
use gunda_http::{
    FinalizeError, HttpClient, PartialDownload, download_to_partial, finalize_download,
    partial_path,
};
use tempfile::tempdir;

fn test_id() -> DownloadId {
    DownloadId::new(42).expect("ID must be valid")
}

async fn prepare_partial(directory: &Path) -> PartialDownload {
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

    let outcome = download_to_partial(&client, &request, test_id(), directory)
        .await
        .expect("partial transfer must succeed");

    let TransferOutcome::Finished(partial) = outcome else {
        panic!("partial transfer must finish without cancellation");
    };

    server.await.expect("server task must succeed");

    partial
}

fn destination(directory: &Path, policy: FileConflictPolicy) -> DownloadDestination {
    DownloadDestination::new(directory.to_path_buf(), Some("file.bin".to_owned()), policy)
}

#[tokio::test]
async fn fail_policy_preserves_existing_output_and_allows_another_choice() {
    let directory = tempdir().expect("temporary directory must exist");
    let target = directory.path().join("file.bin");

    tokio::fs::write(&target, b"existing")
        .await
        .expect("existing output must be created");

    let partial = prepare_partial(directory.path()).await;

    let failure = match finalize_download(
        partial,
        &destination(directory.path(), FileConflictPolicy::Fail),
    )
    .await
    {
        Ok(_) => panic!("existing destination must prevent publication"),
        Err(failure) => failure,
    };

    assert_eq!(
        failure.error(),
        FinalizeError::File(ErrorKind::AlreadyExists),
    );

    assert_eq!(
        tokio::fs::read(&target)
            .await
            .expect("output must be readable"),
        b"existing",
    );

    let (partial, _) = failure.into_parts();
    assert!(partial.path().exists());

    let finalized = finalize_download(
        partial,
        &destination(directory.path(), FileConflictPolicy::Rename),
    )
    .await
    .expect("alternative name must succeed");

    assert_eq!(
        finalized.destination().final_path(),
        directory.path().join("file (1).bin"),
    );

    assert_eq!(
        tokio::fs::read(finalized.destination().final_path())
            .await
            .expect("published output must be readable"),
        b"new",
    );

    assert!(finalized.partial_cleanup_error().is_none());
    assert!(!partial_path(directory.path(), test_id()).exists());
}

#[tokio::test]
async fn overwrite_replaces_existing_output() {
    let directory = tempdir().expect("temporary directory must exist");
    let target = directory.path().join("file.bin");

    tokio::fs::write(&target, b"existing")
        .await
        .expect("existing output must be created");

    let partial = prepare_partial(directory.path()).await;

    let finalized = finalize_download(
        partial,
        &destination(directory.path(), FileConflictPolicy::Overwrite),
    )
    .await
    .expect("explicit overwrite must succeed");

    assert_eq!(finalized.destination().final_path(), target.as_path());
    assert_eq!(finalized.written_bytes(), 3);

    assert_eq!(
        tokio::fs::read(&target)
            .await
            .expect("output must be readable"),
        b"new",
    );

    assert!(!partial_path(directory.path(), test_id()).exists());
}

#[tokio::test]
async fn unsafe_name_preserves_partial_file() {
    let directory = tempdir().expect("temporary directory must exist");
    let partial = prepare_partial(directory.path()).await;

    let destination = DownloadDestination::new(
        directory.path().to_path_buf(),
        Some("../escape.bin".to_owned()),
        FileConflictPolicy::Overwrite,
    );

    let failure = match finalize_download(partial, &destination).await {
        Ok(_) => panic!("unsafe name must be rejected"),
        Err(failure) => failure,
    };

    assert_eq!(failure.error(), FinalizeError::InvalidFilename);

    let (partial, _) = failure.into_parts();

    assert_eq!(
        tokio::fs::read(partial.path())
            .await
            .expect("partial must remain readable"),
        b"new",
    );
}

#[tokio::test]
async fn changed_partial_is_not_published() {
    let directory = tempdir().expect("temporary directory must exist");
    let partial = prepare_partial(directory.path()).await;

    tokio::fs::write(partial.path(), b"changed-length")
        .await
        .expect("test must alter partial");

    let failure = match finalize_download(
        partial,
        &destination(directory.path(), FileConflictPolicy::Fail),
    )
    .await
    {
        Ok(_) => panic!("changed partial must be rejected"),
        Err(failure) => failure,
    };

    assert_eq!(failure.error(), FinalizeError::InvalidPartial);
    assert!(!directory.path().join("file.bin").exists());
}
