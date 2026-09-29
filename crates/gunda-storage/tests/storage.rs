use gunda_core::application::{DownloadManager, DownloadRepository};
use gunda_core::download::{
    DownloadDestination, DownloadOrigin, DownloadProgress, DownloadState, FileConflictPolicy,
    NewDownload, RequestContext, ResolvedDestination, ResourceDescriptor, ResourceKind,
};
use gunda_http::partial_path;
use gunda_storage::{RuntimeLease, SqliteDownloadRepository};
use tempfile::tempdir;
use time::OffsetDateTime;
use url::Url;

#[tokio::test]
async fn startup_recovery_is_durable_and_preservers_files_and_metadata() {
    let directory = tempdir().expect("temporary directory must exitst");
    let db_path = directory.path().join("gunda.sqlite3");

    let _lease = RuntimeLease::acquire(directory.path().join("gunda.runtime.lock"))
        .expect("runtime lease must succeed");

    let repository = SqliteDownloadRepository::open(&db_path)
        .await
        .expect("repository must open");

    let mut originals = Vec::new();
    let mut files = Vec::new();

    for state in [
        DownloadState::Inspecting,
        DownloadState::Downloading,
        DownloadState::Finalizing,
    ] {
        let now = OffsetDateTime::UNIX_EPOCH;

        let mut job = repository
            .create(
                NewDownload::new(
                    RequestContext::new(
                        Url::parse("https://example.com/file.bin").expect("test URL must be valid"),
                        Vec::new(),
                    ),
                    DownloadDestination::new(
                        directory.path().to_path_buf(),
                        None,
                        FileConflictPolicy::Rename,
                    ),
                    DownloadOrigin::Desktop,
                ),
                now,
            )
            .await
            .expect("job must be created");

        job.transition_to(DownloadState::Inspecting, now)
            .expect("inspection transition must succeed");

        if state != DownloadState::Inspecting {
            job.set_resource(
                ResourceDescriptor::new(
                    ResourceKind::File,
                    Some("file.bin".to_owned()),
                    Some("application/octet-stream".to_owned()),
                ),
                now,
            );

            let final_path = directory
                .path()
                .join(format!("output-{}.bin", job.id().value()));

            job.resolve_destination(ResolvedDestination::new(final_path.clone()), now);

            job.transition_to(DownloadState::Downloading, now)
                .expect("download transition must succeed");

            let written = if state == DownloadState::Finalizing {
                10
            } else {
                5
            };

            job.update_progress(
                DownloadProgress::new(written, Some(10)).expect("progress must be valid"),
                now,
            );

            let partial = partial_path(directory.path(), job.id());

            std::fs::write(&partial, b"abc").expect("partial fixture must be written");

            files.push((partial, b"abc".to_vec()));

            if state == DownloadState::Finalizing {
                job.transition_to(DownloadState::Finalizing, now)
                    .expect("finalizing transition must succeed");

                std::fs::write(&final_path, b"0123456789").expect("output fixture must be written");

                files.push((final_path, b"0123456789".to_vec()));
            }
        }

        let persisted = repository
            .save(&job)
            .await
            .expect("active state must be persisted");

        originals.push(persisted);
    }

    repository.close().await;

    let repository = SqliteDownloadRepository::open(&db_path)
        .await
        .expect("repository must reopen");

    let manager = DownloadManager::start(repository)
        .await
        .expect("startup recovery must succeed");

    for original in &originals {
        let recovered = manager
            .job(original.id())
            .expect("recovered job must exist");

        let mut expected = original.snapshot();
        expected.state = DownloadState::Interrupted;
        expected.updated_at = recovered.updated_at();

        assert!(recovered.snapshot() == expected);
        assert!(recovered.updated_at() >= original.updated_at());
    }

    let expected: Vec<_> = manager.jobs().cloned().collect();

    manager.into_repository().close().await;

    let repository = SqliteDownloadRepository::open(&db_path)
        .await
        .expect("repository must reopen again");

    assert!(repository.list().await.expect("listing must succeed") == expected);

    let manager = DownloadManager::start(repository)
        .await
        .expect("repeated startup must succeed");

    assert!(manager.jobs().cloned().collect::<Vec<_>>() == expected);

    for (path, contents) in files {
        assert_eq!(
            std::fs::read(path).expect("existing file must remain readable"),
            contents,
        );
    }

    manager.into_repository().close().await;
}
