use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use gunda_capture::load_token;
use gunda_http::HttpClient;
use gunda_ipc::{
    ClientMessage, Command as IpcCommand, NativePath, Reader, ServerMessage,
    Snapshot as IpcSnapshot, Writer, connect, read_endpoint,
};
use gunda_stream::StreamExecutor;
use tokio::sync::{mpsc, watch};

use super::*;

pub(super) fn start(directory: PathBuf, ctx: egui::Context) -> io::Result<Backend> {
    let (commands, receiver) = mpsc::channel(4);
    let (sender, snapshots) = watch::channel(Snapshot::default());
    let inspector = StreamExecutor::new(HttpClient::new().map_err(io::Error::other)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let closing = Arc::new(AtomicBool::new(false));
    let worker_closing = Arc::clone(&closing);
    let worker = std::thread::Builder::new()
        .name("gunda-client".into())
        .spawn(move || {
            runtime.block_on(run_client(
                directory,
                receiver,
                sender,
                worker_closing,
                ctx,
                inspector,
            ));
        })?;

    Ok(Backend {
        commands: Some(commands),
        snapshots,
        closing,
        worker: Some(worker),
    })
}

async fn run_client(
    directory: PathBuf,
    mut commands: mpsc::Receiver<Command>,
    sender: watch::Sender<Snapshot>,
    closing: Arc<AtomicBool>,
    ctx: egui::Context,
    inspector: StreamExecutor,
) {
    let (capture, capture_problem) = start_capture(&directory).await;
    loop {
        let connected = connect_daemon(&directory).await;
        let daemon = match connected {
            Ok(connected) => connected,
            Err(message) => {
                publish(
                    &sender,
                    &ctx,
                    Snapshot {
                        captures: capture_views(capture.as_ref()),
                        capture: capture.as_ref().map(capture_status),
                        capture_problem,
                        notice: Some(message.into()),
                        ..Default::default()
                    },
                );
                if closing.load(Ordering::Acquire) {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let result = run_connected(
            ClientContext {
                commands: &mut commands,
                sender: &sender,
                closing: &closing,
                ctx: &ctx,
                capture: &capture,
                capture_problem,
                inspector: &inspector,
            },
            daemon,
        )
        .await;

        if closing.load(Ordering::Acquire) {
            return;
        }
        if let Err(message) = result {
            publish(
                &sender,
                &ctx,
                Snapshot {
                    captures: capture_views(capture.as_ref()),
                    capture: capture.as_ref().map(capture_status),
                    capture_problem,
                    notice: Some(message.into()),
                    ..Default::default()
                },
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

struct ClientContext<'a> {
    commands: &'a mut mpsc::Receiver<Command>,
    sender: &'a watch::Sender<Snapshot>,
    closing: &'a AtomicBool,
    ctx: &'a egui::Context,
    capture: &'a Option<CaptureHandle>,
    capture_problem: Option<&'static str>,
    inspector: &'a StreamExecutor,
}

struct ConnectedDaemon {
    reader: Reader,
    writer: Writer,
    initial: IpcSnapshot,
}

async fn run_connected(
    client: ClientContext<'_>,
    daemon: ConnectedDaemon,
) -> Result<(), &'static str> {
    let ClientContext {
        commands,
        sender,
        closing,
        ctx,
        capture,
        capture_problem,
        inspector,
    } = client;
    let ConnectedDaemon {
        mut reader,
        mut writer,
        initial,
    } = daemon;
    let mut snapshot = Snapshot {
        ready: true,
        captures: capture_views(capture.as_ref()),
        capture: capture.as_ref().map(capture_status),
        capture_problem,
        ..Default::default()
    };
    apply_daemon_snapshot(&mut snapshot, initial);
    publish(sender, ctx, snapshot.clone());

    let mut watcher = capture.clone();
    loop {
        enum Wake {
            Command(Option<Box<Command>>),
            Catalog { stopped: bool },
            Daemon(Option<ServerMessage>),
        }

        let wake = tokio::select! {
            command = commands.recv() => Wake::Command(command.map(Box::new)),
            result = catalog_changed(&mut watcher) => Wake::Catalog { stopped: result.is_err() },
            message = reader.receive::<ServerMessage>() => Wake::Daemon(message.ok()),
        };

        match wake {
            Wake::Catalog { stopped } => {
                if stopped {
                    watcher = None;
                }
                snapshot.captures = capture_views(capture.as_ref());
                publish(sender, ctx, snapshot.clone());
            }
            Wake::Daemon(Some(ServerMessage::Snapshot { snapshot: daemon })) => {
                apply_daemon_snapshot(&mut snapshot, daemon);
                publish(sender, ctx, snapshot.clone());
            }
            Wake::Daemon(Some(ServerMessage::Error { message })) => {
                snapshot.notice = Some(message);
                snapshot.busy = false;
                publish(sender, ctx, snapshot.clone());
            }
            Wake::Daemon(None) => return Err("Gundad disconnected."),
            Wake::Command(Some(command)) => {
                if closing.load(Ordering::Acquire) {
                    return Ok(());
                }
                snapshot.busy = true;
                publish(sender, ctx, snapshot.clone());
                if let Some(command) =
                    handle_command(*command, capture, inspector, &mut snapshot).await
                    && writer
                        .send(&ClientMessage::Command { command })
                        .await
                        .is_err()
                {
                    return Err("Could not send the command to Gundad.");
                }
                snapshot.busy = false;
                publish(sender, ctx, snapshot.clone());
            }
            Wake::Command(None) => return Ok(()),
        }
    }
}

async fn handle_command(
    command: Command,
    capture: &Option<CaptureHandle>,
    inspector: &StreamExecutor,
    snapshot: &mut Snapshot,
) -> Option<IpcCommand> {
    match command {
        Command::Start { url, directory } => Some(IpcCommand::Create {
            url: url.as_str().to_owned(),
            directory: NativePath::from_path(&directory).ok()?,
            preferred_filename: None,
            selection: None,
            page_url: None,
            page_title: None,
            engine: None,
        }),
        Command::Capture {
            id,
            directory,
            track,
        } => match captured(capture.as_ref(), id) {
            Some(Ok(captured)) => {
                let (url, filename, selection) = match track {
                    Some(track) => (track.url, track.filename, track.selection),
                    None => (captured.url, None, None),
                };
                let (page_url, page_title) = match captured.origin {
                    DownloadOrigin::Browser {
                        page_url,
                        page_title,
                    } => (Some(page_url.as_str().to_owned()), page_title),
                    _ => (None, None),
                };
                snapshot.capture_tracks = None;
                Some(IpcCommand::Create {
                    url: url.as_str().to_owned(),
                    directory: NativePath::from_path(&directory).ok()?,
                    preferred_filename: filename,
                    selection: selection.map(|selection| selection.as_str().to_owned()),
                    page_url,
                    page_title,
                    engine: Some(
                        match captured.engine {
                            JobEngine::Stream => "stream",
                            JobEngine::Http => "http",
                        }
                        .into(),
                    ),
                })
            }
            Some(Err(message)) => {
                snapshot.notice = Some(message.into());
                None
            }
            None => {
                snapshot.notice = Some("That capture is no longer in the list.".into());
                None
            }
        },
        Command::Inspect(id) => {
            snapshot.capture_tracks = Some(inspect_capture(inspector, capture.as_ref(), id).await);
            None
        }
        Command::DismissCapture(id) => {
            if let Some(handle) = capture
                && handle.remove(id).await.is_err()
            {
                snapshot.notice = Some("The capture could not be dismissed.".into());
            }
            snapshot.captures = capture_views(capture.as_ref());
            None
        }
        Command::ClearCaptures => {
            if let Some(handle) = capture
                && handle.clear().await.is_err()
            {
                snapshot.notice = Some("The capture list could not be cleared.".into());
            }
            snapshot.captures = capture_views(capture.as_ref());
            None
        }
        Command::Pause(id) => Some(IpcCommand::Pause { id: id.value() }),
        Command::Resume(id) => Some(IpcCommand::Resume { id: id.value() }),
        Command::Retry(id) => Some(IpcCommand::Retry { id: id.value() }),
        Command::Restart(id) => Some(IpcCommand::Restart { id: id.value() }),
        Command::Cancel(id) => Some(IpcCommand::Cancel { id: id.value() }),
        Command::SaveStatus(id) => Some(IpcCommand::SaveStatus { id: id.value() }),
    }
}

fn apply_daemon_snapshot(snapshot: &mut Snapshot, daemon: IpcSnapshot) {
    snapshot.jobs = daemon
        .jobs
        .into_iter()
        .filter_map(to_download_view)
        .collect();
    snapshot.active = daemon
        .active
        .into_iter()
        .filter_map(|id| DownloadId::new(id).ok())
        .collect();
    snapshot.notice = daemon.notice;
    snapshot.ready = true;
}

fn to_download_view(job: gunda_ipc::JobView) -> Option<DownloadView> {
    let id = DownloadId::new(job.id).ok()?;
    let state = parse_state(&job.state)?;
    let destination = job.destination.to_path().ok()?;
    let output = match job.output {
        Some(output) => Some(output.to_path().ok()?),
        None => None,
    };
    let error = job.failure_kind.as_deref().map(|kind| {
        let message = failure_message(kind);
        match job.failure_message {
            Some(details) => format!("{message}\nDetails: {details}"),
            None => message.to_owned(),
        }
    });

    Some(DownloadView {
        id,
        name: job.name,
        state,
        written: job.written,
        total: job.total,
        source: job.source,
        origin: match job.origin.as_str() {
            "Desktop" => "Desktop",
            "Browser" => "Browser",
            "CLI" => "CLI",
            _ => "Unknown",
        },
        destination,
        output,
        error,
        retryable: job.retryable,
        resume_available: job.resume_available,
        pause_supported: job.pause_supported,
        completion_pending: job.completion_pending,
    })
}

fn parse_state(state: &str) -> Option<DownloadState> {
    Some(match state {
        "queued" => DownloadState::Queued,
        "inspecting" => DownloadState::Inspecting,
        "downloading" => DownloadState::Downloading,
        "paused" => DownloadState::Paused,
        "finalizing" => DownloadState::Finalizing,
        "completed" => DownloadState::Completed,
        "failed" => DownloadState::Failed,
        "cancelled" => DownloadState::Cancelled,
        "interrupted" => DownloadState::Interrupted,
        _ => return None,
    })
}

fn failure_message(kind: &str) -> &'static str {
    match kind {
        "network" => "Connection problem. Check your internet connection.",
        "authentication" => "The server requires authorization for this download.",
        "remote_rejected" => "The server refused the download.",
        "invalid_response" => "The server returned an unexpected response.",
        "unsupported_resource" => "This type of download is not supported.",
        "permission_denied" => "Gunda cannot save the file in this location.",
        "disk_full" => "There is not enough space to save this file.",
        "integrity" => "The downloaded file could not be verified.",
        "storage" => "Gunda could not save the downloaded file.",
        _ => "The download stopped unexpectedly.",
    }
}

async fn connect_daemon(directory: &Path) -> Result<ConnectedDaemon, &'static str> {
    if let Ok(connected) = try_connect(directory).await {
        return Ok(connected);
    }

    spawn_daemon(directory).map_err(|_| "Could not start Gundad.")?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(connected) = try_connect(directory).await {
            return Ok(connected);
        }
    }

    Err("Could not connect to Gundad.")
}

async fn try_connect(directory: &Path) -> Result<ConnectedDaemon, gunda_ipc::IpcError> {
    let endpoint = read_endpoint(directory)?;
    let token = load_token(&directory.join("gundad-token"))
        .map_err(|_| gunda_ipc::IpcError::Unauthorized)?
        .ok_or(gunda_ipc::IpcError::Unauthorized)?;
    let (connection, snapshot) = connect(&endpoint, token.as_str()).await?;
    let (reader, writer) = connection.into_parts();
    Ok(ConnectedDaemon {
        reader,
        writer,
        initial: snapshot,
    })
}

fn spawn_daemon(directory: &Path) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    ProcessCommand::new(executable)
        .arg("--daemon")
        .arg("--data-dir")
        .arg(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
