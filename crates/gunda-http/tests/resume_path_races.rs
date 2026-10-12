mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::serve_once;
use gunda_core::application::{
    DownloadExecutor, ExecutionInput, PreparedTransfer, RepositoryError, RepositoryErrorKind,
    StagedTransfer, TransferOutcome,
};
use gunda_core::download::{
    DownloadDestination, DownloadId, DownloadProgress, FailureKind, FileConflictPolicy,
    RequestContext, ResolvedDestination, ResourceDescriptor, ResourceKind,
};
use gunda_http::{
    HttpClient, HttpExecutor, HttpResumeState, HttpResumeStore, RestartResetError, StrongEntityTag,
    partial_path,
};
use tempfile::tempdir;
use url::Url;

#[derive(Clone)]
struct TestResumeStore {
    state: Arc<Mutex<Option<HttpResumeState>>>,
    clear_error: Option<RepositoryErrorKind>,
}

impl TestResumeStore {
    fn at(durable_bytes: u64) -> Self {
        let tag = StrongEntityTag::parse(b"\"version-23\"").expect("tag must be valid");
        let state = HttpResumeState::new(tag, 10, durable_bytes).expect("state must be valid");

        Self {
            state: Arc::new(Mutex::new(Some(state))),
            clear_error: None,
        }
    }
}

impl HttpResumeStore for TestResumeStore {
    async fn initialize(
        &self,
        _id: DownloadId,
        strong_etag: &StrongEntityTag,
        total_bytes: u64,
    ) -> Result<HttpResumeState, RepositoryError> {
        let state = HttpResumeState::new(strong_etag.clone(), total_bytes, 0)
            .expect("initialized state must be valid");
        *self.state.lock().expect("store lock must be available") = Some(state.clone());
        Ok(state)
    }

    async fn find(&self, _id: DownloadId) -> Result<Option<HttpResumeState>, RepositoryError> {
        Ok(self
            .state
            .lock()
            .expect("store lock must be available")
            .clone())
    }

    async fn save_checkpoint(
        &self,
        _id: DownloadId,
        state: &HttpResumeState,
    ) -> Result<HttpResumeState, RepositoryError> {
        *self.state.lock().expect("store lock must be available") = Some(state.clone());
        Ok(state.clone())
    }

    async fn clear(&self, _id: DownloadId) -> Result<(), RepositoryError> {
        if let Some(kind) = self.clear_error {
            return Err(RepositoryError::new(kind, "private-storage-marker"));
        }
        *self.state.lock().expect("store lock must be available") = None;
        Ok(())
    }
}

fn test_id() -> DownloadId {
    DownloadId::new(77).expect("ID must be valid")
}

fn resume_input(url: Url, directory: &Path) -> ExecutionInput {
    ExecutionInput {
        id: test_id(),
        request: RequestContext::new(url, Vec::new()),
        destination: DownloadDestination::new(
            directory.to_path_buf(),
            Some("file.bin".to_owned()),
            FileConflictPolicy::Fail,
        ),
        progress: DownloadProgress::new(5, Some(10)).expect("progress must be valid"),
        resource: Some(ResourceDescriptor::new(
            ResourceKind::File,
            Some("file.bin".to_owned()),
            Some("application/octet-stream".to_owned()),
        )),
        resolved_destination: Some(ResolvedDestination::new(directory.join("file.bin"))),
    }
}

async fn prepare_resume(
    directory: &Path,
) -> (
    gunda_http::HttpPreparedTransfer,
    tokio::task::JoinHandle<String>,
) {
    let (url, server) = serve_once(
        "HTTP/1.1 206 Partial Content\r\n\
         Content-Range: bytes 5-9/10\r\n\
         Content-Length: 5\r\n\
         ETag: \"version-23\"\r\n\
         Connection: close\r\n\
         \r\n\
         world",
    )
    .await;
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("client must build"),
        TestResumeStore::at(5),
    );
    let prepared = executor
        .prepare(resume_input(url, directory))
        .await
        .expect("resume preparation must succeed");

    (prepared, server)
}

#[tokio::test]
async fn regular_file_substitution_after_preparation_is_rejected_without_truncation() {
    let directory = tempdir().expect("temporary directory must exist");
    let partial = partial_path(directory.path(), test_id());
    let original = directory.path().join("original-partial");
    tokio::fs::write(&partial, b"helloXYZ")
        .await
        .expect("partial fixture must be written");

    let (prepared, server) = prepare_resume(directory.path()).await;

    tokio::fs::rename(&partial, &original)
        .await
        .expect("original partial must be moved");
    tokio::fs::write(&partial, b"substitute")
        .await
        .expect("substitute must be written");

    let failure = match prepared.transfer().await {
        Ok(_) => panic!("substituted partial must be rejected"),
        Err(failure) => failure,
    };

    assert_eq!(failure.kind(), FailureKind::Integrity);
    assert_eq!(tokio::fs::read(&original).await.unwrap(), b"helloXYZ");
    assert_eq!(tokio::fs::read(&partial).await.unwrap(), b"substitute");
    assert!(!directory.path().join("file.bin").exists());
    server.await.expect("server task must succeed");
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_substitution_does_not_modify_or_publish_its_target() {
    let directory = tempdir().expect("temporary directory must exist");
    let partial = partial_path(directory.path(), test_id());
    let original = directory.path().join("original-partial");
    let target = directory.path().join("target.bin");
    tokio::fs::write(&partial, b"helloXYZ").await.unwrap();
    tokio::fs::write(&target, b"leave-target-alone")
        .await
        .unwrap();

    let (prepared, server) = prepare_resume(directory.path()).await;

    tokio::fs::rename(&partial, &original).await.unwrap();
    std::os::unix::fs::symlink(&target, &partial).expect("symlink must be created");

    let failure = match prepared.transfer().await {
        Ok(_) => panic!("symlink substitution must be rejected"),
        Err(failure) => failure,
    };

    assert_eq!(failure.kind(), FailureKind::Integrity);
    assert_eq!(tokio::fs::read(&original).await.unwrap(), b"helloXYZ");
    assert_eq!(
        tokio::fs::read(&target).await.unwrap(),
        b"leave-target-alone"
    );
    assert!(!directory.path().join("file.bin").exists());
    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn ordinary_resume_truncates_only_the_uncheckpointed_suffix() {
    let directory = tempdir().expect("temporary directory must exist");
    let partial = partial_path(directory.path(), test_id());
    tokio::fs::write(&partial, b"helloXYZ")
        .await
        .expect("partial fixture must be written");

    let (prepared, server) = prepare_resume(directory.path()).await;
    let outcome = prepared.transfer().await.expect("resume must succeed");
    let TransferOutcome::Finished(staged) = outcome else {
        panic!("resume must finish");
    };

    assert_eq!(tokio::fs::read(&partial).await.unwrap(), b"helloworld");

    let output = staged.finalize().await.expect("publication must succeed");
    assert_eq!(
        tokio::fs::read(output.destination.final_path())
            .await
            .unwrap(),
        b"helloworld"
    );
    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn substitution_after_transfer_is_not_published() {
    let directory = tempdir().expect("temporary directory must exist");
    let partial = partial_path(directory.path(), test_id());
    let completed = directory.path().join("completed-partial");
    tokio::fs::write(&partial, b"helloXYZ").await.unwrap();

    let (prepared, server) = prepare_resume(directory.path()).await;
    let outcome = prepared.transfer().await.expect("resume must succeed");
    let TransferOutcome::Finished(staged) = outcome else {
        panic!("resume must finish");
    };

    tokio::fs::rename(&partial, &completed).await.unwrap();
    tokio::fs::write(&partial, b"substitute").await.unwrap();

    let failure = match staged.finalize().await {
        Ok(_) => panic!("substituted staging path must not be published"),
        Err(failure) => failure,
    };

    assert_eq!(failure.kind(), FailureKind::Integrity);
    assert_eq!(tokio::fs::read(&completed).await.unwrap(), b"helloworld");
    assert_eq!(tokio::fs::read(&partial).await.unwrap(), b"substitute");
    assert!(!directory.path().join("file.bin").exists());
    server.await.expect("server task must succeed");
}

#[tokio::test]
async fn restart_removes_the_partial_and_clears_its_resume_state() {
    let directory = tempdir().expect("temporary directory must exist");
    let id = test_id();
    let partial = partial_path(directory.path(), id);
    std::fs::write(&partial, b"old partial bytes").expect("partial must be created");
    let store = TestResumeStore::at(5);
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("HTTP client must build"),
        store.clone(),
    );

    executor
        .reset_for_restart(id, directory.path())
        .await
        .expect("safe partial must be discarded");

    assert!(!partial.exists());
    assert_eq!(store.find(id).await.expect("lookup must succeed"), None);
}

#[tokio::test]
async fn restart_reports_file_failure_without_clearing_resume_state() {
    let directory = tempdir().expect("temporary directory must exist");
    let id = test_id();
    let partial = partial_path(directory.path(), id);
    std::fs::create_dir(&partial).expect("unsafe staging directory must exist");
    let store = TestResumeStore::at(5);
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("HTTP client must build"),
        store.clone(),
    );

    let error = executor
        .reset_for_restart(id, directory.path())
        .await
        .unwrap_err();

    assert_eq!(
        error,
        RestartResetError::PartialFile(std::io::ErrorKind::InvalidInput)
    );
    assert!(partial.is_dir());
    assert!(store.find(id).await.unwrap().is_some());
    assert!(!format!("{error} {error:?}").contains(directory.path().to_string_lossy().as_ref()));
}

#[tokio::test]
async fn restart_reports_storage_failure_after_removing_partial() {
    let directory = tempdir().expect("temporary directory must exist");
    let id = test_id();
    let partial = partial_path(directory.path(), id);
    std::fs::write(&partial, b"old partial bytes").expect("partial must be created");
    let mut store = TestResumeStore::at(5);
    store.clear_error = Some(RepositoryErrorKind::Unavailable);
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("HTTP client must build"),
        store.clone(),
    );

    let error = executor
        .reset_for_restart(id, directory.path())
        .await
        .unwrap_err();

    assert_eq!(
        error,
        RestartResetError::ResumeStorage(RepositoryErrorKind::Unavailable)
    );
    assert!(!partial.exists());
    assert!(store.find(id).await.unwrap().is_some());
    assert!(!format!("{error} {error:?}").contains("private-storage-marker"));
}

#[cfg(unix)]
#[tokio::test]
async fn restart_rejects_symlink_without_touching_its_target_or_state() {
    let directory = tempdir().expect("temporary directory must exist");
    let target = directory.path().join("user-file.bin");
    let partial = partial_path(directory.path(), test_id());
    std::fs::write(&target, b"must survive").expect("target must be created");
    std::os::unix::fs::symlink(&target, &partial).expect("symlink must be created");
    let store = TestResumeStore::at(5);
    let executor = HttpExecutor::with_resume_store(
        HttpClient::new().expect("HTTP client must build"),
        store.clone(),
    );

    assert!(
        executor
            .reset_for_restart(test_id(), directory.path())
            .await
            .is_err()
    );

    assert_eq!(std::fs::read(&target).unwrap(), b"must survive");
    assert!(
        std::fs::symlink_metadata(&partial)
            .expect("unsafe staging entry must remain untouched")
            .file_type()
            .is_symlink()
    );
    assert!(
        store
            .find(test_id())
            .await
            .expect("lookup must succeed")
            .is_some()
    );
}
