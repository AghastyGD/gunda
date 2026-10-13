use super::*;

#[tokio::test]
async fn updating_one_job_preserves_other_jobs_recovery_actions() {
    let repository = SqliteDownloadRepository::open_in_memory().await.unwrap();
    let mut manager = DownloadManager::start(repository).await.unwrap();
    for _ in 0..2 {
        manager
            .create(NewDownload::new(
                RequestContext::new(
                    Url::parse("https://example.com/file.bin").unwrap(),
                    Vec::new(),
                ),
                DownloadDestination::new("Downloads".into(), None, FileConflictPolicy::Rename),
                DownloadOrigin::Desktop,
            ))
            .await
            .unwrap();
    }
    let mut saved = crate::view::tests::sample(DownloadState::Finalizing);
    saved.completion_pending = true;
    saved.output = Some("Downloads/file (1).bin".into());
    saved.name = "file (1).bin".into();
    let mut snapshot = Snapshot {
        jobs: vec![saved],
        ..Default::default()
    };
    let second = manager.jobs().nth(1).unwrap();
    update_job_view(&mut snapshot, second);
    assert!(snapshot.jobs[0].completion_pending);
    assert_eq!(snapshot.jobs[0].name, "file (1).bin");
    assert_eq!(
        snapshot.jobs[0].output.as_deref(),
        Some(std::path::Path::new("Downloads/file (1).bin"))
    );
    assert_eq!(snapshot.jobs[1].id, second.id());
    assert_eq!(snapshot.jobs.len(), 2);
    update_job_view(&mut snapshot, second);
    assert_eq!(snapshot.jobs.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn nonresumable_body_failure_retries_from_zero_successfully() {
    let data = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nhello".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nhelloworld"
                .as_slice(),
        ] {
            let (mut socket, _) = timeout(Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap();
            requests.push(read_request(&mut socket).await);
            socket.write_all(response).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        requests
    });
    let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
    wait_for(&mut backend, |snapshot| snapshot.ready).await;
    backend
        .send(Command::Start {
            url: Url::parse(&format!("http://{address}/file.bin")).unwrap(),
            directory: output.path().into(),
        })
        .unwrap();
    wait_for(&mut backend, |snapshot| {
        snapshot
            .notice
            .as_deref()
            .is_some_and(|message| message.contains("Trying again"))
    })
    .await;
    let completed = wait_for(&mut backend, |snapshot| {
        !snapshot.busy
            && snapshot
                .jobs
                .first()
                .is_some_and(|view| view.state == DownloadState::Completed)
    })
    .await;
    let view = &completed.jobs[0];
    let repository = SqliteDownloadRepository::open(data.path().join("gunda.sqlite3"))
        .await
        .unwrap();
    let job = repository.find_by_id(view.id).await.unwrap().unwrap();
    assert_eq!(job.state(), DownloadState::Completed);
    assert!(!partial_path(output.path(), view.id).exists());
    assert!(
        repository
            .http_resume_store()
            .find(view.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        std::fs::read(view.output.as_ref().unwrap()).unwrap(),
        b"helloworld"
    );
    assert_eq!(view.written, 10);
    assert_eq!(server.await.unwrap().len(), 2);
}
#[tokio::test(flavor = "multi_thread")]
async fn published_file_status_can_be_saved_without_another_download() {
    let data = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let (url, server) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"version-1\"\r\nConnection: close\r\n\r\nhello").await;
    // Force finalization to choose a name other than its planned path.
    let previous = output.path().join("file.bin");
    std::fs::write(&previous, b"existing output").unwrap();
    let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
    wait_for(&mut backend, |snapshot| snapshot.ready).await;
    let database = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(data.path().join("gunda.sqlite3")),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER reject_completed BEFORE UPDATE OF state ON downloads WHEN NEW.state = 'completed' BEGIN SELECT RAISE(ABORT, 'fixture completion failure'); END")
        .execute(&database).await.unwrap();
    backend
        .send(Command::Start {
            url,
            directory: output.path().into(),
        })
        .unwrap();
    let stopped = wait_for(&mut backend, |snapshot| {
        !snapshot.busy
            && snapshot
                .jobs
                .first()
                .is_some_and(|view| view.completion_pending)
    })
    .await;
    let view = &stopped.jobs[0];
    let id = view.id;
    let published = view.output.clone().unwrap();
    assert_eq!(view.name, published.file_name().unwrap().to_string_lossy());
    assert_eq!(view.state, DownloadState::Finalizing);
    assert_eq!(view.label(false), "File saved");
    assert_ne!(published, previous);
    assert_eq!(std::fs::read(&published).unwrap(), b"hello");
    assert!(!view.can_resume() && !view.can_retry() && !view.can_restart());
    assert!(stopped.active.is_none());
    let request = String::from_utf8(server.await.unwrap()).unwrap();
    assert!(request.starts_with("GET "));
    backend.send(Command::SaveStatus(id)).unwrap();
    let failed = wait_for(&mut backend, |snapshot| {
        !snapshot.busy
            && snapshot
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("could not be updated"))
    })
    .await;
    assert!(failed.jobs[0].completion_pending);
    assert_eq!(failed.jobs[0].output.as_ref(), Some(&published));
    sqlx::query("DROP TRIGGER reject_completed")
        .execute(&database)
        .await
        .unwrap();
    backend.send(Command::SaveStatus(id)).unwrap();
    let completed = wait_for(&mut backend, |snapshot| {
        !snapshot.busy
            && snapshot
                .jobs
                .first()
                .is_some_and(|view| view.state == DownloadState::Completed)
    })
    .await;
    assert!(!completed.jobs[0].completion_pending);
    assert_eq!(completed.jobs[0].output.as_ref(), Some(&published));
    assert_eq!(std::fs::read(&published).unwrap(), b"hello");
    assert_eq!(std::fs::read(&previous).unwrap(), b"existing output");
    database.close().await;
    drop(backend);
    let mut reopened = Backend::start(data.path().into(), egui::Context::default()).unwrap();
    let history = wait_for(&mut reopened, |snapshot| snapshot.ready).await;
    assert_eq!(history.jobs[0].state, DownloadState::Completed);
    assert_eq!(history.jobs[0].output.as_ref(), Some(&published));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_resume_data_offers_restart_and_rejects_stale_resume_commands() {
    for saved_checkpoint in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let (url, server) =
            serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello").await;
        let repository = SqliteDownloadRepository::open(data.path().join("gunda.sqlite3"))
            .await
            .unwrap();
        let mut manager = DownloadManager::start(repository).await.unwrap();
        let id = manager
            .create(NewDownload::new(
                RequestContext::new(url, Vec::new()),
                DownloadDestination::new(output.path().into(), None, FileConflictPolicy::Rename),
                DownloadOrigin::Desktop,
            ))
            .await
            .unwrap()
            .download_id();
        let mut snapshot = manager.job(id).unwrap().snapshot();
        snapshot.state = DownloadState::Interrupted;
        let previous = output.path().join("file.bin");
        if saved_checkpoint {
            snapshot.progress = DownloadProgress::new(5, Some(5)).unwrap();
            snapshot.resource = Some(ResourceDescriptor::new(ResourceKind::File, None, None));
            snapshot.resolved_destination = Some(gunda_core::download::ResolvedDestination::new(
                previous.clone(),
            ));
            std::fs::write(&previous, b"prior output").unwrap();
        }
        let repository = manager.into_repository();
        repository
            .save(&DownloadJob::restore(snapshot))
            .await
            .unwrap();
        if saved_checkpoint {
            let store = repository.http_resume_store();
            let tag = StrongEntityTag::parse(b"\"old-version\"").unwrap();
            store.initialize(id, &tag, 5).await.unwrap();
            store
                .save_checkpoint(id, &HttpResumeState::new(tag, 5, 5).unwrap())
                .await
                .unwrap();
        }
        let mut backend = Backend::start(data.path().into(), egui::Context::default()).unwrap();
        let interrupted = wait_for(&mut backend, |snapshot| snapshot.ready).await;
        assert!(!interrupted.jobs[0].can_resume());
        assert!(interrupted.jobs[0].can_restart());
        assert!(
            interrupted.jobs[0]
                .error
                .as_deref()
                .unwrap()
                .contains("Restart")
        );
        backend.send(Command::Resume(id)).unwrap();
        let rejected = wait_for(&mut backend, |snapshot| {
            !snapshot.busy
                && snapshot
                    .notice
                    .as_deref()
                    .is_some_and(|notice| notice.contains("Use Restart"))
        })
        .await;
        assert_eq!(rejected.jobs[0].state, DownloadState::Interrupted);
        backend.send(Command::Restart(id)).unwrap();
        let completed = wait_for(&mut backend, |snapshot| {
            !snapshot.busy
                && snapshot
                    .jobs
                    .first()
                    .is_some_and(|view| view.state == DownloadState::Completed)
        })
        .await;
        assert_eq!(
            std::fs::read(completed.jobs[0].output.as_ref().unwrap()).unwrap(),
            b"hello"
        );
        let request = String::from_utf8(server.await.unwrap())
            .unwrap()
            .to_ascii_lowercase();
        assert!(!request.contains("\r\nrange:"));
        if saved_checkpoint {
            assert_eq!(std::fs::read(&previous).unwrap(), b"prior output");
        }
    }
}
