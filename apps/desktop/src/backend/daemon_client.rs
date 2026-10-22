use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use gunda_capture::load_token;
use gunda_ipc::{
    CaptureTrackChoice as IpcTrackChoice, ClientMessage, Command as IpcCommand, NativePath, Reader,
    ServerMessage, Snapshot as IpcSnapshot, Writer, connect, read_endpoint,
};
use tokio::sync::{mpsc, watch};

use super::*;

pub(super) fn start(directory: PathBuf, ctx: egui::Context) -> io::Result<Backend> {
    let (commands, receiver) = mpsc::channel(4);
    let (sender, snapshots) = watch::channel(Snapshot::default());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let closing = Arc::new(AtomicBool::new(false));
    let worker_closing = Arc::clone(&closing);
    let worker = std::thread::Builder::new()
        .name("gunda-client".into())
        .spawn(move || {
            runtime.block_on(run_client(directory, receiver, sender, worker_closing, ctx))
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
) {
    loop {
        let connected = connect_daemon(&directory).await;
        let daemon = match connected {
            Ok(connected) => connected,
            Err(message) => {
                publish(
                    &sender,
                    &ctx,
                    Snapshot {
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
    } = client;
    let ConnectedDaemon {
        mut reader,
        mut writer,
        initial,
    } = daemon;
    let mut snapshot = Snapshot::default();
    apply_daemon_snapshot(&mut snapshot, initial);
    publish(sender, ctx, snapshot.clone());

    loop {
        enum Wake {
            Command(Option<Box<Command>>),
            Daemon(Option<Box<ServerMessage>>),
        }

        let wake = tokio::select! {
            command = commands.recv() => Wake::Command(command.map(Box::new)),
            message = reader.receive::<ServerMessage>() => Wake::Daemon(message.ok().map(Box::new)),
        };

        match wake {
            Wake::Daemon(Some(message)) => match *message {
                ServerMessage::Snapshot { snapshot: daemon } => {
                    apply_daemon_snapshot(&mut snapshot, daemon);
                    publish(sender, ctx, snapshot.clone());
                }
                ServerMessage::Error { message } => {
                    snapshot.notice = Some(message);
                    snapshot.busy = false;
                    publish(sender, ctx, snapshot.clone());
                }
            },
            Wake::Daemon(None) => return Err("Gundad disconnected."),
            Wake::Command(Some(command)) => {
                if closing.load(Ordering::Acquire) {
                    return Ok(());
                }
                snapshot.busy = true;
                publish(sender, ctx, snapshot.clone());
                if let Some(command) = handle_command(*command, &mut snapshot)
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

fn handle_command(command: Command, snapshot: &mut Snapshot) -> Option<IpcCommand> {
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
        } => {
            snapshot.capture_tracks = None;
            Some(IpcCommand::DownloadCapture {
                id,
                directory: NativePath::from_path(&directory).ok()?,
                track: track.map(|track| IpcTrackChoice {
                    url: track.url.as_str().to_owned(),
                    filename: track.filename,
                    selection: track
                        .selection
                        .map(|selection| selection.as_str().to_owned()),
                }),
            })
        }
        Command::Inspect(id) => Some(IpcCommand::InspectCapture { id }),
        Command::DismissCapture(id) => Some(IpcCommand::DismissCapture { id }),
        Command::ClearCaptures => Some(IpcCommand::ClearCaptures),
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
    snapshot.captures = daemon.captures.into_iter().map(to_capture_view).collect();
    snapshot.capture_tracks = daemon.capture_tracks.and_then(to_capture_tracks);
    snapshot.capture = daemon.capture.map(|capture| {
        Arc::new(CaptureStatus {
            endpoint: capture.endpoint,
            token: capture.token,
        })
    });
    snapshot.capture_problem = daemon.capture_problem;
    snapshot.notice = daemon.notice;
    snapshot.ready = true;
}

fn to_capture_view(capture: gunda_ipc::CaptureView) -> CaptureView {
    CaptureView {
        id: capture.id,
        url: capture.url,
        kind: match capture.kind.as_str() {
            "HLS" => "HLS",
            "DASH" => "DASH",
            "File" => "File",
            _ => "Unknown",
        },
        content_type: capture.content_type,
        page_title: capture.page_title,
        seen: capture.seen,
        segmented: capture.segmented,
    }
}

fn to_capture_tracks(tracks: gunda_ipc::CaptureTracks) -> Option<CaptureTracks> {
    Some(CaptureTracks {
        id: tracks.id,
        tracks: tracks
            .tracks
            .into_iter()
            .filter_map(|track| {
                let url = Url::parse(&track.url).ok()?;
                let selection = track
                    .selection
                    .map(ExecutionSelection::new)
                    .transpose()
                    .ok()?;

                Some(TrackView {
                    label: track.label,
                    details: track.details,
                    url,
                    filename: track.filename,
                    selection,
                })
            })
            .collect(),
        problem: tracks.problem,
    })
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
