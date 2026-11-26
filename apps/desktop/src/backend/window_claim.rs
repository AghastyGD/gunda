//! Claims a Gunda window through the daemon so it is never shown twice.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, mpsc as std_mpsc};
use std::time::Duration;

use eframe::egui;
use gunda_core::download::DownloadId;
#[cfg(not(test))]
use gunda_ipc::{ClientMessage, Command as IpcCommand, ServerMessage};

#[cfg(not(test))]
use super::daemon_client::{ConnectedDaemon, connect_daemon};

/// How long a starting window waits to learn whether it is already shown.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(15);

/// The outcome of asking to show a window.
pub enum WindowClaim {
    /// This process shows the window for as long as it runs.
    Owned(WindowLease),
    /// Another process already shows the window and was asked to raise it.
    Shown,
}

/// Brings this process's window forward when it is opened again.
pub struct WindowLease {
    window: Arc<OnceLock<egui::Context>>,
}

impl WindowLease {
    /// Connects the lease to the window it raises.
    pub fn attach(&self, ctx: &egui::Context) {
        let _ = self.window.set(ctx.clone());
    }
}

/// Claims the window for one download, or the main window when `download` is absent.
///
/// When Gundad cannot be reached the window opens anyway, since showing it is
/// more useful than refusing.
pub fn claim_window(directory: PathBuf, download: Option<DownloadId>) -> WindowClaim {
    let window = Arc::new(OnceLock::new());
    let (claimed, outcome) = std_mpsc::channel();
    let held = Arc::clone(&window);
    let spawned = std::thread::Builder::new()
        .name("gunda-window".into())
        .spawn(move || hold_window(directory, download, claimed, held));

    if spawned.is_ok() && matches!(outcome.recv_timeout(CLAIM_TIMEOUT), Ok(false)) {
        return WindowClaim::Shown;
    }
    WindowClaim::Owned(WindowLease { window })
}

#[cfg(test)]
fn hold_window(
    _directory: PathBuf,
    _download: Option<DownloadId>,
    claimed: std_mpsc::Sender<bool>,
    _window: Arc<OnceLock<egui::Context>>,
) {
    let _ = claimed.send(true);
}

#[cfg(not(test))]
fn hold_window(
    directory: PathBuf,
    download: Option<DownloadId>,
    claimed: std_mpsc::Sender<bool>,
    window: Arc<OnceLock<egui::Context>>,
) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        let _ = claimed.send(true);
        return;
    };
    runtime.block_on(async move {
        let download = download.map(DownloadId::value);
        let mut claimed = Some(claimed);
        loop {
            match connect_daemon(&directory).await {
                Ok(daemon) => {
                    if !serve_claim(daemon, download, &mut claimed, &window).await {
                        return;
                    }
                }
                Err(_) => {
                    if let Some(claimed) = claimed.take() {
                        let _ = claimed.send(true);
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

/// Holds one claim until the connection ends.
///
/// Returns `false` when another process shows the window, so the claim stops.
#[cfg(not(test))]
async fn serve_claim(
    daemon: ConnectedDaemon,
    download: Option<i64>,
    claimed: &mut Option<std_mpsc::Sender<bool>>,
    window: &OnceLock<egui::Context>,
) -> bool {
    let ConnectedDaemon {
        mut reader,
        mut writer,
        ..
    } = daemon;
    let claim = ClientMessage::Command {
        command: IpcCommand::HostWindow { download },
    };
    if writer.send(&claim).await.is_err() {
        return true;
    }

    while let Ok(message) = reader.receive::<ServerMessage>().await {
        match message {
            ServerMessage::WindowHosted { accepted } => {
                if let Some(claimed) = claimed.take() {
                    let _ = claimed.send(accepted);
                }
                if !accepted {
                    return false;
                }
            }
            ServerMessage::ShowWindow { .. } => {
                if let Some(ctx) = window.get() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.request_repaint();
                }
            }
            ServerMessage::Snapshot { .. } | ServerMessage::Error { .. } => {}
        }
    }
    true
}
