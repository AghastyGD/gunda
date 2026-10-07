use gunda_core::application::{DownloadRepository, RepositoryErrorKind};
use gunda_core::download::{
    DownloadDestination, DownloadId, DownloadOrigin, FileConflictPolicy, NewDownload,
    RequestContext,
};
use gunda_http::{HttpResumeState, HttpResumeStore, StrongEntityTag};
use gunda_storage::SqliteDownloadRepository;
use tempfile::tempdir;
use time::OffsetDateTime;
use url::Url;

fn tag(value: &[u8]) -> StrongEntityTag {
    StrongEntityTag::parse(value).expect("test tag must be valid")
}

async fn create_job(repository: &SqliteDownloadRepository) -> DownloadId {
    repository
        .create(
            NewDownload::new(
                RequestContext::new(
                    Url::parse("https://sample.com/file.bin").expect("test URL must be valid"),
                    Vec::new(),
                ),
                DownloadDestination::new(
                    "downloads".into(),
                    Some("file.bin".to_owned()),
                    FileConflictPolicy::Rename,
                ),
                DownloadOrigin::Desktop,
            ),
            OffsetDateTime::UNIX_EPOCH,
        )
        .await
        .expect("job must be created")
        .id()
}

#[tokio::test]
async fn identity_and_checkpoint_survive_repository_reopen() {
    let directory = tempdir().expect("temporary directory must exist");
    let path = directory.path().join("gunda.sqlite3");

    let repository = SqliteDownloadRepository::open(&path)
        .await
        .expect("repository must open");

    let id = create_job(&repository).await;
    let store = repository.http_resume_store();
    let validator = tag(b"\"version-\x80\xff\"");

    assert_eq!(store.find(id).await.expect("lookup must succeed"), None);

    let initial = store
        .initialize(id, &validator, 10)
        .await
        .expect("initialization must succeed");

    assert_eq!(initial.durable_bytes(), 0);

    let expected = HttpResumeState::new(validator, 10, 5).expect("checkpoint must be valid");

    assert_eq!(
        store
            .save_checkpoint(id, &expected)
            .await
            .expect("checkpoint must be saved"),
        expected,
    );

    drop(store);
    repository.close().await;

    let repository = SqliteDownloadRepository::open(&path)
        .await
        .expect("repository must reopen");

    let store = repository.http_resume_store();

    assert_eq!(
        store.find(id).await.expect("lookup must succeed"),
        Some(expected),
    );

    drop(store);
    repository.close().await;
}

#[tokio::test]
async fn initialization_cannot_replace_existing_resume_state() {
    let repository = SqliteDownloadRepository::open_in_memory()
        .await
        .expect("repository must open");

    let id = create_job(&repository).await;
    let store = repository.http_resume_store();

    let expected = store
        .initialize(id, &tag(b"\"original\""), 10)
        .await
        .expect("initialization must succeed");

    let error = store
        .initialize(id, &tag(b"\"replacement\""), 20)
        .await
        .expect_err("existing state must not be replaced");

    assert_eq!(error.kind(), RepositoryErrorKind::ConstraintViolation);
    assert_eq!(
        store.find(id).await.expect("lookup must succeed"),
        Some(expected),
    );

    drop(store);
    repository.close().await;
}

#[tokio::test]
async fn checkpoint_updates_preserve_identity_and_are_monotonic() {
    let repository = SqliteDownloadRepository::open_in_memory()
        .await
        .expect("repository must open");

    let id = create_job(&repository).await;
    let store = repository.http_resume_store();
    let validator = tag(b"\"original\"");

    store
        .initialize(id, &validator, 10)
        .await
        .expect("initialization must succeed");

    let expected =
        HttpResumeState::new(validator.clone(), 10, 5).expect("checkpoint must be valid");

    store
        .save_checkpoint(id, &expected)
        .await
        .expect("checkpoint must advance");

    store
        .save_checkpoint(id, &expected)
        .await
        .expect("repeated checkpoint must succeed");

    for rejected in [
        HttpResumeState::new(validator.clone(), 10, 4)
            .expect("lower checkpoint must be representable"),
        HttpResumeState::new(tag(b"\"different\""), 10, 6)
            .expect("different identity must be representable"),
        HttpResumeState::new(validator, 11, 6).expect("different total must be representable"),
    ] {
        let error = store
            .save_checkpoint(id, &rejected)
            .await
            .expect_err("incompatible checkpoint must be rejected");

        assert_eq!(error.kind(), RepositoryErrorKind::ConstraintViolation);
        assert_eq!(
            store.find(id).await.expect("lookup must succeed"),
            Some(expected.clone()),
        );
    }

    drop(store);
    repository.close().await;
}

#[tokio::test]
async fn checkpoint_requires_initialized_resume_state() {
    let repository = SqliteDownloadRepository::open_in_memory()
        .await
        .expect("repository must open");

    let id = create_job(&repository).await;
    let store = repository.http_resume_store();

    let state =
        HttpResumeState::new(tag(b"\"original\""), 10, 5).expect("checkpoint must be valid");

    let error = store
        .save_checkpoint(id, &state)
        .await
        .expect_err("missing resume state must be rejected");

    assert_eq!(error.kind(), RepositoryErrorKind::NotFound);

    drop(store);
    repository.close().await;
}

#[tokio::test]
async fn counts_outside_sqlite_range_are_rejected_without_insertion() {
    let repository = SqliteDownloadRepository::open_in_memory()
        .await
        .expect("repository must open");

    let id = create_job(&repository).await;
    let store = repository.http_resume_store();

    let error = store
        .initialize(id, &tag(b"\"original\""), u64::MAX)
        .await
        .expect_err("unsupported total must be rejected");

    assert_eq!(error.kind(), RepositoryErrorKind::ConstraintViolation);
    assert_eq!(store.find(id).await.expect("lookup must succeed"), None);

    drop(store);
    repository.close().await;
}
