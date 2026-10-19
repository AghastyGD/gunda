use gunda_core::application::{DownloadRepository, RepositoryErrorKind};
use gunda_core::download::{
    DownloadDestination, DownloadOrigin, FileConflictPolicy, NewDownload, RequestContext,
};
use gunda_storage::SqliteDownloadRepository;
use gunda_stream::{PlanFingerprint, StreamResumeState, StreamResumeStore, TrackCheckpoint};
use tempfile::tempdir;
use time::OffsetDateTime;
use url::Url;

#[tokio::test]
async fn stream_checkpoints_survive_reopen_and_only_advance() {
    let directory = tempdir().expect("temporary directory must exist");
    let database = directory.path().join("gunda.sqlite3");
    let repository = SqliteDownloadRepository::open(&database)
        .await
        .expect("repository must open");
    let job = repository
        .create(
            NewDownload::new(
                RequestContext::new(
                    Url::parse("https://example.com/master.m3u8").unwrap(),
                    Vec::new(),
                ),
                DownloadDestination::new(
                    directory.path().to_path_buf(),
                    None,
                    FileConflictPolicy::Rename,
                ),
                DownloadOrigin::Desktop,
            ),
            OffsetDateTime::UNIX_EPOCH,
        )
        .await
        .expect("job must be created");
    let fingerprint = PlanFingerprint::parse(&[7_u8; 16]).unwrap();
    let store = repository.stream_resume_store();
    let initial = store
        .initialize(job.id(), fingerprint, 2)
        .await
        .expect("resume state must initialize");

    assert_eq!(initial.fingerprint(), fingerprint);
    assert_eq!(initial.tracks().len(), 2);

    let advanced = StreamResumeState::new(
        fingerprint,
        vec![TrackCheckpoint::new(3, 30), TrackCheckpoint::new(1, 8)],
    )
    .unwrap();
    store
        .save_checkpoint(job.id(), &advanced)
        .await
        .expect("checkpoint must advance");
    repository.close().await;

    let repository = SqliteDownloadRepository::open(&database)
        .await
        .expect("repository must reopen");
    let store = repository.stream_resume_store();
    let found = store
        .find(job.id())
        .await
        .expect("lookup must succeed")
        .expect("checkpoint must survive reopen");

    assert_eq!(found, advanced);

    let backwards = StreamResumeState::new(
        fingerprint,
        vec![TrackCheckpoint::new(2, 20), TrackCheckpoint::new(1, 8)],
    )
    .unwrap();
    let error = store
        .save_checkpoint(job.id(), &backwards)
        .await
        .expect_err("a checkpoint must not move backwards");

    assert_eq!(error.kind(), RepositoryErrorKind::ConstraintViolation);

    store.clear(job.id()).await.expect("clear must succeed");
    assert!(store.find(job.id()).await.unwrap().is_none());
}
