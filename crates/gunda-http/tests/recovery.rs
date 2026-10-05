use std::path::Path;

use gunda_core::download::{
    DownloadDestination, DownloadId, DownloadJob, DownloadOrigin, DownloadProgress, DownloadState,
    FileConflictPolicy, NewDownload, RequestContext, ResolvedDestination,
};
use gunda_http::{
    CheckpointComparison, LocalFileState, RecoveryInspectionError, inspect_local_recovery,
    partial_path,
};
use tempfile::tempdir;
use time::OffsetDateTime;
use url::Url;

fn interrupted_job(directory: &Path) -> DownloadJob {
    let now = OffsetDateTime::UNIX_EPOCH;

    let job = DownloadJob::new(
        DownloadId::new(1).expect("test ID must be valid"),
        NewDownload::new(
            RequestContext::new(
                Url::parse("https://example.com/file.bin").expect("test URL must be valid"),
                Vec::new(),
            ),
            DownloadDestination::new(
                directory.to_path_buf(),
                Some("file.bin".to_owned()),
                FileConflictPolicy::Rename,
            ),
            DownloadOrigin::Desktop,
        ),
        now,
    );

    let mut snapshot = job.snapshot();
    snapshot.state = DownloadState::Interrupted;
    snapshot.progress = DownloadProgress::new(5, Some(10)).expect("progress must be valid");

    DownloadJob::restore(snapshot)
}

#[tokio::test]
async fn missing_partial_is_not_reported_as_an_empty_file() {
    let directory = tempdir().expect("temporary directory must exist");
    let job = interrupted_job(directory.path());

    let inspection = inspect_local_recovery(&job)
        .await
        .expect("inspection must succeed");

    assert_eq!(inspection.partial, LocalFileState::Missing);
    assert_eq!(inspection.planned_output, None);
    assert_eq!(inspection.checkpoint_bytes, 5);
    assert_eq!(inspection.total_bytes, Some(10));
    assert_eq!(
        inspection.checkpoint_comparison(),
        CheckpointComparison::Unavailable,
    );

    assert!(!partial_path(directory.path(), job.id()).exists());
}

#[tokio::test]
async fn inspection_compares_lengths_without_changing_data() {
    for (length, comparison) in [
        (0_usize, CheckpointComparison::FileShorter),
        (3, CheckpointComparison::FileShorter),
        (5, CheckpointComparison::SameLength),
        (8, CheckpointComparison::FileLonger),
        (12, CheckpointComparison::FileLonger),
    ] {
        let directory = tempdir().expect("temporary directory must exist");
        let job = interrupted_job(directory.path());
        let before = job.snapshot();
        let path = partial_path(directory.path(), job.id());
        let contents = vec![b'x'; length];

        std::fs::write(&path, &contents).expect("partial must be written");

        let inspection = inspect_local_recovery(&job)
            .await
            .expect("inspection must succeed");

        assert_eq!(
            inspection.partial,
            LocalFileState::RegularFile {
                bytes: length as u64,
            },
        );
        assert_eq!(inspection.checkpoint_comparison(), comparison);
        assert_eq!(inspection.checkpoint_bytes, 5);
        assert_eq!(inspection.total_bytes, Some(10));

        assert!(job.snapshot() == before);
        assert_eq!(
            std::fs::read(&path).expect("partial must remain readable"),
            contents,
        );
    }
}

#[tokio::test]
async fn planned_output_is_observed_without_completing_the_job() {
    let directory = tempdir().expect("temporary directory must exist");
    let mut job = interrupted_job(directory.path());
    let output = directory.path().join("file.bin");

    job.resolve_destination(
        ResolvedDestination::new(output.clone()),
        OffsetDateTime::UNIX_EPOCH,
    );

    let missing = inspect_local_recovery(&job)
        .await
        .expect("inspection must succeed");

    assert_eq!(missing.planned_output, Some(LocalFileState::Missing));

    std::fs::write(&output, b"0123456789").expect("output fixture must be written");

    let before = job.snapshot();

    let present = inspect_local_recovery(&job)
        .await
        .expect("inspection must succeed");

    assert_eq!(
        present.planned_output,
        Some(LocalFileState::RegularFile { bytes: 10 }),
    );
    assert_eq!(job.state(), DownloadState::Interrupted);
    assert!(job.snapshot() == before);
    assert_eq!(
        std::fs::read(&output).expect("output must remain readable"),
        b"0123456789",
    );
}

#[tokio::test]
async fn directory_at_partial_path_is_not_a_regular_file() {
    let directory = tempdir().expect("temporary directory must exist");
    let job = interrupted_job(directory.path());
    let path = partial_path(directory.path(), job.id());

    std::fs::create_dir(&path).expect("directory fixture must be created");

    let inspection = inspect_local_recovery(&job)
        .await
        .expect("inspection must succeed");

    assert_eq!(inspection.partial, LocalFileState::Other);
    assert_eq!(
        inspection.checkpoint_comparison(),
        CheckpointComparison::Unavailable,
    );
    assert!(path.is_dir());
}

#[tokio::test]
async fn running_job_cannot_be_inspected_for_restart_recovery() {
    let directory = tempdir().expect("temporary directory must exist");
    let mut snapshot = interrupted_job(directory.path()).snapshot();
    snapshot.state = DownloadState::Downloading;

    let job = DownloadJob::restore(snapshot);

    assert_eq!(
        inspect_local_recovery(&job).await,
        Err(RecoveryInspectionError::NotInterrupted),
    );
}

#[cfg(unix)]
#[tokio::test]
async fn partial_symlink_is_reported_without_following_its_target() {
    let directory = tempdir().expect("temporary directory must exist");
    let job = interrupted_job(directory.path());
    let target = directory.path().join("target.bin");
    let partial = partial_path(directory.path(), job.id());

    std::fs::write(&target, b"keep me").expect("target must be written");
    std::os::unix::fs::symlink(&target, &partial).expect("symlink must be created");

    let inspection = inspect_local_recovery(&job)
        .await
        .expect("inspection must succeed");

    assert_eq!(inspection.partial, LocalFileState::Symlink);
    assert_eq!(
        std::fs::read(&target).expect("target must remain readable"),
        b"keep me",
    );
}
