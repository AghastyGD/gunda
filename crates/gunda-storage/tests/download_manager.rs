use std::path::PathBuf;

use gunda_core::application::{DownloadEvent, DownloadManager, RepositoryErrorKind};
use gunda_core::download::{
    DownloadDestination, DownloadOrigin, FileConflictPolicy, HeaderSensitivity, NewDownload,
    RequestContext, RequestHeader,
};
use gunda_storage::SqliteDownloadRepository;
use tempfile::tempdir;
use url::Url;

fn new_download(filename: &str, headers: Vec<RequestHeader>) -> NewDownload {
    NewDownload::new(
        RequestContext::new(
            Url::parse("https://example.com/file.bin").expect("test URL must be valid"),
            headers,
        ),
        DownloadDestination::new(
            PathBuf::from("downloads").join("images"),
            Some(filename.to_owned()),
            FileConflictPolicy::Rename,
        ),
        DownloadOrigin::Desktop,
    )
}

#[tokio::test]
async fn manager_restores_complete_jobs_after_restart() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must open");

    let mut manager = DownloadManager::start(repository)
        .await
        .expect("manager must start");

    assert!(manager.is_empty());

    let inputs = [
        new_download(
            "first.bin",
            vec![
                RequestHeader::new(
                    "Accept",
                    "application/octet-stream",
                    HeaderSensitivity::Public,
                ),
                RequestHeader::new("Accept", "application/zip", HeaderSensitivity::Public),
            ],
        ),
        new_download("second.bin", Vec::new()),
        new_download(
            "third.bin",
            vec![RequestHeader::new(
                "User-Agent",
                "Gunda test",
                HeaderSensitivity::Public,
            )],
        ),
    ];

    let mut expected = Vec::new();

    for input in inputs {
        let event = manager
            .create(input.clone())
            .await
            .expect("creation must succeed");

        let DownloadEvent::Created { id } = event else {
            panic!("creation must return a Created event");
        };

        let job = manager.job(id).expect("created job must be visible");

        assert!(job.request() == input.request());
        assert!(job.destination() == input.destination());

        expected.push(job.clone());
    }

    manager.into_repository().close().await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");

    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must restart");

    let restored: Vec<_> = manager.jobs().cloned().collect();

    assert!(restored == expected);

    manager.into_repository().close().await;
}

#[tokio::test]
async fn rejected_creation_preserves_jobs_in_memory_and_on_disk() {
    let directory = tempdir().expect("temporary directory must exist");
    let database_path = directory.path().join("gunda.sqlite3");

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must open");

    let mut manager = DownloadManager::start(repository)
        .await
        .expect("manager must start");

    manager
        .create(new_download("existing.bin", Vec::new()))
        .await
        .expect("public download must be created");

    let expected: Vec<_> = manager.jobs().cloned().collect();

    let result = manager
        .create(new_download(
            "rejected.bin",
            vec![RequestHeader::new(
                "Authorization",
                "Bearer test-secret",
                HeaderSensitivity::Sensitive,
            )],
        ))
        .await;

    let error = match result {
        Ok(_) => panic!("sensitive request must not produce a success event"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), RepositoryErrorKind::SensitiveDataUnsupported);
    assert!(manager.jobs().cloned().collect::<Vec<_>>() == expected);

    manager.into_repository().close().await;

    let repository = SqliteDownloadRepository::open(&database_path)
        .await
        .expect("repository must reopen");

    let manager = DownloadManager::start(repository)
        .await
        .expect("manager must restart");

    assert!(manager.jobs().cloned().collect::<Vec<_>>() == expected);

    manager.into_repository().close().await;
}
