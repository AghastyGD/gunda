//! Hands files and windows to the operating system.

use std::env;
#[cfg(windows)]
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use gunda_core::download::DownloadId;

/// Opens a downloaded file with its default application.
pub fn open_file(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let command = {
        let mut command = Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(windows)]
    let command = {
        let mut command = Command::new("explorer");
        command.arg(path);
        command
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let command = {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    };

    spawn_detached(command)
}

/// Shows a downloaded file in its folder.
pub fn show_in_folder(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let command = {
        let mut command = Command::new("open");
        command.arg("-R").arg(path);
        command
    };
    #[cfg(windows)]
    let command = {
        use std::os::windows::process::CommandExt as _;

        // Explorer reads `/select,"path"` as one argument and rejects the
        // quoting the standard library would apply.
        let mut argument = OsString::from("/select,\"");
        argument.push(path.as_os_str());
        argument.push("\"");
        let mut command = Command::new("explorer");
        command.raw_arg(argument);
        command
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let command = {
        let mut command = Command::new("xdg-open");
        command.arg(path.parent().unwrap_or(path));
        command
    };

    spawn_detached(command)
}

/// Starts the window that follows one download.
///
/// A window that is already open is raised instead, by the process started here.
pub fn launch_download_window(directory: &Path, id: DownloadId) -> io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("--download-window")
        .arg(id.value().to_string())
        .arg("--data-dir")
        .arg(directory);
    spawn_detached(command)
}

fn spawn_detached(mut command: Command) -> io::Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reap the child so a finished helper does not linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}
