use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gunda_core::application::{
    DownloadCancellation, DownloadExecutor, DownloadManager, DownloadRepository, ExecutionInput,
    PreparedTransfer, RepositoryError, RepositoryErrorKind, StagedTransfer, TransferOutcome,
    TransferProgress,
};
use gunda_core::download::{
    DownloadDestination, DownloadId, DownloadJob, DownloadOrigin, DownloadProgress, DownloadState,
    FileConflictPolicy, NewDownload, RequestContext,
};
use gunda_http::HttpClient;
use gunda_stream::{
    PlanFingerprint, StreamExecutor, StreamResumeState, StreamResumeStore, TrackCheckpoint,
};
use tempfile::tempdir;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use url::Url;

const PLAYLIST: &str = "#EXTM3U\n\
#EXTINF:1,\ns0.ts\n\
#EXTINF:1,\ns1.ts\n\
#EXTINF:1,\ns2.ts\n\
#EXT-X-ENDLIST\n";

#[derive(Clone, Default)]
struct MemoryStore {
    state: Arc<Mutex<Option<StreamResumeState>>>,
}

impl StreamResumeStore for MemoryStore {
    async fn initialize(
        &self,
        _id: DownloadId,
        fingerprint: PlanFingerprint,
        track_count: usize,
    ) -> Result<StreamResumeState, RepositoryError> {
        let mut slot = self.state.lock().expect("the test lock is not poisoned");

        if slot.is_some() {
            return Err(RepositoryError::new(
                RepositoryErrorKind::ConstraintViolation,
                "state already exists",
            ));
        }

        let state = StreamResumeState::new(
            fingerprint,
            (0..track_count).map(|_| TrackCheckpoint::start()).collect(),
        )
        .map_err(|_| RepositoryError::new(RepositoryErrorKind::InvalidData, "bad state"))?;
        *slot = Some(state.clone());
        Ok(state)
    }

    async fn find(&self, _id: DownloadId) -> Result<Option<StreamResumeState>, RepositoryError> {
        Ok(self
            .state
            .lock()
            .expect("the test lock is not poisoned")
            .clone())
    }

    async fn save_checkpoint(
        &self,
        _id: DownloadId,
        state: &StreamResumeState,
    ) -> Result<StreamResumeState, RepositoryError> {
        let mut stored = self.state.lock().expect("the test lock is not poisoned");
        let current = stored
            .as_ref()
            .ok_or_else(|| RepositoryError::new(RepositoryErrorKind::NotFound, "bad state"))?;

        if current.fingerprint() != state.fingerprint()
            || current.tracks().len() != state.tracks().len()
            || state
                .tracks()
                .iter()
                .zip(current.tracks())
                .any(|(next, previous)| {
                    next.next_resource() < previous.next_resource()
                        || next.durable_bytes() < previous.durable_bytes()
                })
        {
            return Err(RepositoryError::new(
                RepositoryErrorKind::ConstraintViolation,
                "checkpoint moves backwards",
            ));
        }

        *stored = Some(state.clone());
        Ok(state.clone())
    }

    async fn clear(&self, _id: DownloadId) -> Result<(), RepositoryError> {
        *self.state.lock().expect("the test lock is not poisoned") = None;
        Ok(())
    }
}

#[tokio::test]
async fn a_paused_segmented_download_resumes_at_the_last_complete_segment() {
    let directory = tempdir().unwrap();
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let base = serve(
        vec![
            ("/media.m3u8", PLAYLIST.as_bytes().to_vec()),
            ("/s0.ts", b"SEG0".to_vec()),
            ("/s1.ts", b"SEG1".to_vec()),
            ("/s2.ts", b"SEG2".to_vec()),
        ],
        events,
        "/s1.ts",
    )
    .await;
    let entry = base.join("media.m3u8").unwrap();
    let store = MemoryStore::default();
    let executor = StreamExecutor::with_resume_store(
        HttpClient::new().expect("client must build"),
        store.clone(),
    );
    let first = executor
        .prepare(input(entry.clone(), directory.path()))
        .await
        .expect("preparation must succeed");
    assert!(first.supports_pause());
    let cancellation = DownloadCancellation::new();
    cancellation.set_pause_supported(true);
    let control = cancellation.clone();
    let transfer = tokio::spawn(async move {
        first
            .transfer_controlled(TransferProgress::disabled(), control)
            .await
    });

    let mut paused_after_s1 = false;
    while let Some(path) = event_rx.recv().await {
        if path == "/s1.ts" {
            cancellation.request_pause();
            paused_after_s1 = true;
            break;
        }
    }
    assert!(paused_after_s1, "the transfer never reached s1");

    let outcome = transfer.await.unwrap().expect("pause must be orderly");
    assert!(matches!(outcome, TransferOutcome::Stopped { .. }));

    let state = store
        .find(DownloadId::new(1).unwrap())
        .await
        .unwrap()
        .expect("the pause leaves a checkpoint");
    assert_eq!(state.tracks()[0].next_resource(), 1);
    assert_eq!(state.tracks()[0].durable_bytes(), 4);

    let resumed = executor
        .prepare(input(entry, directory.path()))
        .await
        .expect("resume preparation must succeed");
    assert_eq!(resumed.starting_bytes(), 4);

    let outcome = resumed.transfer().await.expect("resume must finish");
    let TransferOutcome::Finished(staged) = outcome else {
        panic!("resume must finish");
    };
    let output = staged.finalize().await.expect("publication must succeed");

    assert_eq!(
        std::fs::read(output.destination.final_path()).unwrap(),
        b"SEG0SEG1SEG2"
    );
}

const MUX_MASTER: &str = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360,AUDIO=\"aac\"\n\
video.m3u8\n";

const MUX_VIDEO: &str = "#EXTM3U\n\
#EXT-X-MAP:URI=\"v-init.mp4\"\n\
#EXTINF:1,\nv0.m4s\n\
#EXTINF:1,\nv1.m4s\n\
#EXT-X-ENDLIST\n";

const MUX_AUDIO: &str = "#EXTM3U\n\
#EXT-X-MAP:URI=\"a-init.mp4\"\n\
#EXTINF:1,\na0.m4s\n\
#EXT-X-ENDLIST\n";

#[tokio::test]
async fn a_paused_split_presentation_resumes_both_tracks_before_muxing() {
    let directory = tempdir().unwrap();
    let video_init = initialization(b"vide", 30_000);
    let video_first = segment(0, &[(0xa1, 4)], 1000);
    let video_second = segment(1000, &[(0xa2, 4)], 1000);
    let audio_init = initialization(b"soun", 44_100);
    let audio_only = segment(0, &[(0xb1, 8)], 1024);
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let base = serve(
        vec![
            ("/master.m3u8", MUX_MASTER.as_bytes().to_vec()),
            ("/video.m3u8", MUX_VIDEO.as_bytes().to_vec()),
            ("/audio.m3u8", MUX_AUDIO.as_bytes().to_vec()),
            ("/v-init.mp4", video_init),
            ("/v0.m4s", video_first),
            ("/v1.m4s", video_second),
            ("/a-init.mp4", audio_init),
            ("/a0.m4s", audio_only),
        ],
        events,
        "/v1.m4s",
    )
    .await;
    let entry = base.join("master.m3u8").unwrap();
    let store = MemoryStore::default();
    let executor = StreamExecutor::with_resume_store(
        HttpClient::new().expect("client must build"),
        store.clone(),
    );
    let first = executor
        .prepare(input(entry.clone(), directory.path()))
        .await
        .expect("preparation must succeed");
    assert!(first.supports_pause());
    let cancellation = DownloadCancellation::new();
    cancellation.set_pause_supported(true);
    let control = cancellation.clone();
    let transfer = tokio::spawn(async move {
        first
            .transfer_controlled(TransferProgress::disabled(), control)
            .await
    });

    let mut paused_after_v1 = false;
    while let Some(path) = event_rx.recv().await {
        if path == "/v1.m4s" {
            cancellation.request_pause();
            paused_after_v1 = true;
            break;
        }
    }
    assert!(paused_after_v1, "the transfer never reached v1");

    let outcome = transfer.await.unwrap().expect("pause must be orderly");
    assert!(matches!(outcome, TransferOutcome::Stopped { .. }));

    let state = store
        .find(DownloadId::new(1).unwrap())
        .await
        .unwrap()
        .expect("the pause leaves a checkpoint");
    assert_eq!(state.tracks().len(), 2);
    assert_eq!(state.tracks()[0].next_resource(), 2);
    assert_eq!(state.tracks()[1].next_resource(), 0);

    let resumed = executor
        .prepare(input(entry, directory.path()))
        .await
        .expect("resume preparation must succeed");
    let outcome = resumed.transfer().await.expect("resume must finish");
    let TransferOutcome::Finished(staged) = outcome else {
        panic!("resume must finish");
    };
    let output = staged.finalize().await.expect("publication must succeed");
    let bytes = std::fs::read(output.destination.final_path()).unwrap();

    assert!(position_of(&bytes, &[0xa1; 4]).is_some());
    assert!(position_of(&bytes, &[0xa2; 4]).is_some());
    assert!(position_of(&bytes, &[0xb1; 8]).is_some());

    let leftovers: Vec<_> = std::fs::read_dir(directory.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains(".part"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "unexpected staging files: {leftovers:?}"
    );
}

#[tokio::test]
async fn a_changed_plan_is_refused_instead_of_resumed() {
    let directory = tempdir().unwrap();
    let (events, _event_rx) = mpsc::unbounded_channel();
    let base = serve(
        vec![("/media.m3u8", PLAYLIST.as_bytes().to_vec())],
        events,
        "/s1.ts",
    )
    .await;
    let entry = base.join("media.m3u8").unwrap();
    let store = MemoryStore::default();
    let fingerprint = PlanFingerprint::parse(&[9_u8; 16]).unwrap();
    store
        .initialize(DownloadId::new(1).unwrap(), fingerprint, 1)
        .await
        .unwrap();
    let executor =
        StreamExecutor::with_resume_store(HttpClient::new().expect("client must build"), store);

    let error = match executor.prepare(input(entry, directory.path())).await {
        Ok(_) => panic!("a different plan must not resume"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), gunda_core::download::FailureKind::Integrity);
}

#[tokio::test]
async fn restart_cleanup_removes_segmented_staging_and_state() {
    let directory = tempdir().unwrap();
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let base = serve(
        vec![
            ("/media.m3u8", PLAYLIST.as_bytes().to_vec()),
            ("/s0.ts", b"SEG0".to_vec()),
            ("/s1.ts", b"SEG1".to_vec()),
            ("/s2.ts", b"SEG2".to_vec()),
        ],
        events,
        "/s1.ts",
    )
    .await;
    let entry = base.join("media.m3u8").unwrap();
    let store = MemoryStore::default();
    let executor = StreamExecutor::with_resume_store(
        HttpClient::new().expect("client must build"),
        store.clone(),
    );
    let prepared = executor
        .prepare(input(entry, directory.path()))
        .await
        .unwrap();
    let cancellation = DownloadCancellation::new();
    cancellation.set_pause_supported(true);
    let control = cancellation.clone();
    let transfer = tokio::spawn(async move {
        prepared
            .transfer_controlled(TransferProgress::disabled(), control)
            .await
    });

    while let Some(path) = event_rx.recv().await {
        if path == "/s1.ts" {
            cancellation.request_pause();
            break;
        }
    }
    let outcome = transfer.await.unwrap().unwrap();
    assert!(matches!(outcome, TransferOutcome::Stopped { .. }));

    executor
        .reset_for_restart(DownloadId::new(1).unwrap(), directory.path())
        .await
        .expect("restart cleanup must succeed");

    assert!(
        store
            .find(DownloadId::new(1).unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!directory.path().join(".gunda-1.part").exists());
}

#[derive(Default)]
struct MemoryRepository {
    jobs: Mutex<Vec<DownloadJob>>,
    next_id: AtomicI64,
}

impl DownloadRepository for MemoryRepository {
    async fn create(
        &self,
        download: NewDownload,
        created_at: OffsetDateTime,
    ) -> Result<DownloadJob, RepositoryError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let id = DownloadId::new(id).expect("the fake ID is valid");
        let job = DownloadJob::new(id, download, created_at);
        self.jobs.lock().unwrap().push(job.clone());
        Ok(job)
    }

    async fn find_by_id(&self, id: DownloadId) -> Result<Option<DownloadJob>, RepositoryError> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .find(|job| job.id() == id)
            .cloned())
    }

    async fn list(&self) -> Result<Vec<DownloadJob>, RepositoryError> {
        Ok(self.jobs.lock().unwrap().clone())
    }

    async fn save(&self, job: &DownloadJob) -> Result<DownloadJob, RepositoryError> {
        let mut jobs = self.jobs.lock().unwrap();
        let current = jobs
            .iter_mut()
            .find(|current| current.id() == job.id())
            .ok_or_else(|| RepositoryError::new(RepositoryErrorKind::NotFound, "missing job"))?;
        *current = job.clone();
        Ok(current.clone())
    }
}

#[tokio::test]
async fn a_muxed_download_completes_through_the_manager() {
    let directory = tempdir().unwrap();
    let video_init = initialization(b"vide", 30_000);
    let video = segment(0, &[(0xa1, 4)], 1000);
    let audio_init = initialization(b"soun", 44_100);
    let audio = segment(0, &[(0xb1, 8)], 1024);
    let (events, _event_rx) = mpsc::unbounded_channel();
    let base = serve(
        vec![
            ("/master.m3u8", MUX_MASTER.as_bytes().to_vec()),
            ("/video.m3u8", MUX_VIDEO.as_bytes().to_vec()),
            ("/audio.m3u8", MUX_AUDIO.as_bytes().to_vec()),
            ("/v-init.mp4", video_init),
            ("/v0.m4s", video),
            ("/v1.m4s", segment(1000, &[(0xa2, 4)], 1000)),
            ("/a-init.mp4", audio_init),
            ("/a0.m4s", audio),
        ],
        events,
        "/missing",
    )
    .await;
    let entry = base.join("master.m3u8").unwrap();
    let store = MemoryStore::default();
    let executor = StreamExecutor::with_resume_store(
        HttpClient::new().expect("client must build"),
        store.clone(),
    );
    let manager = DownloadManager::start(MemoryRepository::default())
        .await
        .expect("manager must start");
    let id = manager
        .create(NewDownload::new(
            RequestContext::new(entry, Vec::new()),
            DownloadDestination::new(
                directory.path().to_path_buf(),
                None,
                FileConflictPolicy::Fail,
            ),
            DownloadOrigin::Desktop,
        ))
        .await
        .expect("job must be created")
        .download_id();

    let report = manager
        .execute(id, &executor)
        .await
        .expect("the muxed download must complete");
    let job = manager.job(id).unwrap();

    assert!(matches!(
        report.event,
        gunda_core::application::DownloadEvent::Completed { .. }
    ));
    assert_eq!(job.state(), DownloadState::Completed);
    let output = job
        .resolved_destination()
        .expect("the published path is recorded")
        .final_path();
    let bytes = std::fs::read(output).unwrap();
    assert!(position_of(&bytes, &[0xa1; 4]).is_some());
    assert!(position_of(&bytes, &[0xa2; 4]).is_some());
    assert!(position_of(&bytes, &[0xb1; 8]).is_some());
    assert!(store.find(id).await.unwrap().is_none());
}

fn input(url: Url, directory: &Path) -> ExecutionInput {
    ExecutionInput {
        id: DownloadId::new(1).expect("ID must be valid"),
        request: RequestContext::new(url, Vec::new()),
        destination: DownloadDestination::new(
            directory.to_path_buf(),
            None,
            FileConflictPolicy::Fail,
        ),
        progress: DownloadProgress::default(),
        resource: None,
        resolved_destination: None,
    }
}

async fn serve(
    routes: Vec<(&str, Vec<u8>)>,
    events: mpsc::UnboundedSender<String>,
    delayed: &str,
) -> Url {
    let routes: Arc<HashMap<String, Vec<u8>>> = Arc::new(
        routes
            .into_iter()
            .map(|(path, body)| (path.to_owned(), body))
            .collect(),
    );
    let delayed = delayed.to_owned();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let routes = Arc::clone(&routes);
            let events = events.clone();
            let delayed = delayed.clone();

            tokio::spawn(async move {
                let mut socket = socket;
                let Ok(head) = read_head(&mut socket).await else {
                    return;
                };
                let mut parts = head.split_whitespace();
                let method = parts.next().unwrap_or_default();
                let path = parts.next().unwrap_or("/");

                if method == "GET" {
                    let _ = events.send(path.to_owned());
                }

                if path == delayed && method == "GET" {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }

                let body = routes.get(path).cloned().unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: video/mp2t\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });

    Url::parse(&format!("http://{address}/")).unwrap()
}

fn container_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(body.len().saturating_add(8))
        .expect("a fixture box fits in a file")
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut payload = vec![version];
    payload.extend_from_slice(&flags.to_be_bytes()[1..]);
    payload.extend_from_slice(body);
    container_box(kind, &payload)
}

fn initialization(handler: &[u8; 4], timescale: u32) -> Vec<u8> {
    let (media_header, entry) = if handler == b"vide" {
        (
            full_box(b"vmhd", 0, 1, &[0; 8]),
            container_box(b"avc1", &[0; 78]),
        )
    } else {
        (
            full_box(b"smhd", 0, 0, &[0; 4]),
            container_box(b"mp4a", &[0; 28]),
        )
    };

    let mut stsd = vec![0, 0, 0, 0];
    stsd.extend_from_slice(&1_u32.to_be_bytes());
    stsd.extend_from_slice(&entry);

    let mut stbl = container_box(b"stsd", &stsd);
    stbl.extend_from_slice(&full_box(b"stts", 0, 0, &[]));
    stbl.extend_from_slice(&full_box(b"stsc", 0, 0, &[]));

    let mut minf = media_header;
    minf.extend_from_slice(&container_box(
        b"dinf",
        &full_box(b"dref", 0, 0, &[0, 0, 0, 1, 0, 0, 0, 1]),
    ));
    minf.extend_from_slice(&container_box(b"stbl", &stbl));

    let mut tkhd = vec![0_u8; 80];
    tkhd[8..12].copy_from_slice(&1_u32.to_be_bytes());

    let mut mdhd = vec![0_u8; 20];
    mdhd[8..12].copy_from_slice(&timescale.to_be_bytes());

    let mut hdlr = vec![0_u8; 4];
    hdlr.extend_from_slice(handler);

    let mut mdia = full_box(b"mdhd", 0, 0, &mdhd);
    mdia.extend_from_slice(&full_box(b"hdlr", 0, 0, &hdlr));
    mdia.extend_from_slice(&container_box(b"minf", &minf));

    let mut trak = full_box(b"tkhd", 0, 7, &tkhd);
    trak.extend_from_slice(&container_box(b"mdia", &mdia));

    let mut file = container_box(b"ftyp", b"iso5\x00\x00\x02\x00iso5iso6mp41");
    file.extend_from_slice(&container_box(b"moov", &container_box(b"trak", &trak)));

    file
}

fn segment(decode_time: u64, samples: &[(u8, u32)], sample_duration: u32) -> Vec<u8> {
    let describe = |data_offset: i32| {
        let mut traf = full_box(b"tfhd", 0, 0x020000, &1_u32.to_be_bytes());
        traf.extend_from_slice(&full_box(b"tfdt", 1, 0, &decode_time.to_be_bytes()));

        let mut run = u32::try_from(samples.len())
            .expect("a fixture segment holds few samples")
            .to_be_bytes()
            .to_vec();
        run.extend_from_slice(&data_offset.to_be_bytes());

        for (_, size) in samples {
            run.extend_from_slice(&sample_duration.to_be_bytes());
            run.extend_from_slice(&size.to_be_bytes());
            run.extend_from_slice(&0x0200_0000_u32.to_be_bytes());
            run.extend_from_slice(&0_i32.to_be_bytes());
        }

        traf.extend_from_slice(&full_box(
            b"trun",
            1,
            0x0001 | 0x0100 | 0x0200 | 0x0400 | 0x0800,
            &run,
        ));

        container_box(b"moof", &container_box(b"traf", &traf))
    };

    let mut moof = describe(0);
    let offset = i32::try_from(moof.len().saturating_add(8)).expect("a small fragment");
    moof = describe(offset);

    let mut data = Vec::new();
    for (byte, size) in samples {
        data.extend(std::iter::repeat_n(*byte, *size as usize));
    }
    moof.extend_from_slice(&container_box(b"mdat", &data));

    moof
}

fn position_of(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn read_head(socket: &mut TcpStream) -> io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];

    loop {
        let read = socket.read(&mut byte).await?;
        if read == 0 {
            break;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    Ok(String::from_utf8_lossy(&head).into_owned())
}
