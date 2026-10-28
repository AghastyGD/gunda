use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use gunda_capture::{
    endpoint_path as capture_endpoint_path, load_endpoint as load_capture_endpoint,
    load_token as load_capture_token, token_path as capture_token_path,
};
use gunda_ipc::{
    NATIVE_HOST_NAME, NATIVE_HOST_PROTOCOL_VERSION, NativeHostRequest, NativeHostResponse,
    read_endpoint, read_native_host_request, write_chrome_native_host_manifest,
    write_native_host_response,
};

const CHROME_EXTENSION_ID: &str = "mpjbdgojfmnkhfmlcggfdephcjcoklce";
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const START_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_os = "linux")]
const CHROMIUM_CONFIGS: &[&str] = &[
    "google-chrome",
    "chromium",
    "BraveSoftware/Brave-Browser",
    "microsoft-edge",
    "microsoft-edge-beta",
];

#[cfg(any(windows, test))]
const WINDOWS_CHROMIUM_HOSTS: &[&str] = &[
    r"Software\Google\Chrome\NativeMessagingHosts",
    r"Software\Chromium\NativeMessagingHosts",
    r"Software\Microsoft\Edge\NativeMessagingHosts",
    r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
];

pub fn install(directory: &Path) -> Result<(), Box<dyn Error>> {
    let directory = std::path::absolute(directory)?;
    fs::create_dir_all(&directory)?;

    #[cfg(target_os = "linux")]
    {
        let executable = env::current_exe()?;
        let host = launcher(&directory, &executable)?;
        let config = directories::BaseDirs::new()
            .ok_or("Could not find the browser configuration folder.")?
            .config_dir()
            .to_path_buf();
        install_chromium_manifests(&config, &host, CHROME_EXTENSION_ID)?;
    }

    #[cfg(windows)]
    {
        let executable = env::current_exe()?;
        let host = windows_launcher(&directory, &executable)?;
        install_windows_manifests(&directory, &host, CHROME_EXTENSION_ID)?;
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    let _ = directory;

    Ok(())
}

pub fn run(directory: PathBuf) -> Result<(), Box<dyn Error>> {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let request = match read_native_host_request(&mut stdin) {
        Ok(request) => request,
        Err(_) => {
            write_native_host_response(
                &mut stdout,
                &NativeHostResponse::Error {
                    message: "The native host request could not be read.".into(),
                },
            )?;
            return Ok(());
        }
    };

    let response = match request {
        NativeHostRequest::Pairing { version, .. } => {
            if version != NATIVE_HOST_PROTOCOL_VERSION {
                NativeHostResponse::Error {
                    message: "The extension uses an unsupported native host protocol.".into(),
                }
            } else {
                match ensure_pairing(&directory) {
                    Ok((endpoint, token)) => NativeHostResponse::Pairing {
                        version: NATIVE_HOST_PROTOCOL_VERSION,
                        endpoint,
                        token,
                    },
                    Err(message) => NativeHostResponse::Error { message },
                }
            }
        }
        NativeHostRequest::ChooseDirectory { version } => {
            if version != NATIVE_HOST_PROTOCOL_VERSION {
                NativeHostResponse::Error {
                    message: "The extension uses an unsupported native host protocol.".into(),
                }
            } else {
                match choose_directory().and_then(|path| path.to_str().map(str::to_owned)) {
                    Some(path) => NativeHostResponse::Directory { path },
                    None => NativeHostResponse::Error {
                        message: "No folder was selected.".into(),
                    },
                }
            }
        }
    };
    write_native_host_response(&mut stdout, &response)?;
    Ok(())
}

fn choose_directory() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = Command::new("zenity")
            .args([
                "--file-selection",
                "--directory",
                "--title=Choose a Gunda download folder",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            if output.status.success() {
                let path = String::from_utf8(output.stdout).ok()?;
                return Some(PathBuf::from(path.trim()));
            }
            return None;
        }
    }

    rfd::FileDialog::new()
        .set_title("Choose a Gunda download folder")
        .pick_folder()
}

#[cfg(target_os = "linux")]
fn launcher(directory: &Path, executable: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let host = directory.join("gunda-native-host");
    let executable = executable
        .to_str()
        .ok_or("The Gunda executable path is not valid Unicode.")?;
    let directory = directory
        .to_str()
        .ok_or("The Gunda data path is not valid Unicode.")?;
    let script = format!(
        "#!/bin/sh\nexec {} --native-host --data-dir {}\n",
        shell_quote(executable),
        shell_quote(directory),
    );
    fs::write(&host, script)?;

    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(&host, fs::Permissions::from_mode(0o700))?;

    Ok(host)
}

#[cfg(target_os = "linux")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(windows)]
fn windows_launcher(directory: &Path, executable: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let host = directory.join("gunda-native-host.cmd");
    let executable = executable
        .to_str()
        .ok_or("The Gunda executable path is not valid Unicode.")?;
    let directory = directory
        .to_str()
        .ok_or("The Gunda data path is not valid Unicode.")?;
    fs::write(&host, batch_script(executable, directory))?;
    Ok(host)
}

#[cfg(any(windows, test))]
fn batch_script(executable: &str, directory: &str) -> String {
    // Chrome launches a non-executable host through cmd.exe. The code page
    // change keeps non-ASCII profile paths readable in the next line.
    format!(
        "@echo off\r\nsetlocal DisableDelayedExpansion\r\nchcp 65001 >nul 2>&1\r\n\"{}\" --native-host --data-dir \"{}\"\r\n",
        batch_value(executable),
        batch_value(directory),
    )
}

/// Escapes the one character cmd.exe expands inside a quoted argument.
#[cfg(any(windows, test))]
fn batch_value(value: &str) -> String {
    value.replace('%', "%%")
}

#[cfg(windows)]
fn install_windows_manifests(
    directory: &Path,
    host: &Path,
    extension_id: &str,
) -> Result<(), Box<dyn Error>> {
    let manifest = directory.join(format!("{NATIVE_HOST_NAME}.json"));
    write_chrome_native_host_manifest(&manifest, host, extension_id)?;

    for browser in WINDOWS_CHROMIUM_HOSTS {
        let status = registration_status(windows_registration_arguments(browser, &manifest))?;
        if !status.success() {
            return Err(format!("The native host could not be registered for {browser}.").into());
        }
    }

    Ok(())
}

#[cfg(any(windows, test))]
fn windows_registration_arguments(browser: &str, manifest: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "add".into(),
        format!(r"HKCU\{browser}\{NATIVE_HOST_NAME}").into(),
        "/ve".into(),
        "/t".into(),
        "REG_SZ".into(),
        "/d".into(),
        manifest.as_os_str().to_owned(),
        "/f".into(),
    ]
}

/// Runs `reg.exe` without opening a console window.
#[cfg(windows)]
fn registration_status(arguments: Vec<std::ffi::OsString>) -> io::Result<std::process::ExitStatus> {
    let mut command = Command::new("reg");
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    hide_window(&mut command).status()
}

/// Keeps a console application from opening a visible window.
#[cfg(windows)]
fn hide_window(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    command.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(target_os = "linux")]
fn install_chromium_manifests(
    config: &Path,
    host: &Path,
    extension_id: &str,
) -> Result<(), Box<dyn Error>> {
    for browser in CHROMIUM_CONFIGS {
        let root = config.join(browser);
        if !root.is_dir() {
            continue;
        }

        let directory = root.join("NativeMessagingHosts");
        fs::create_dir_all(&directory)?;
        write_chrome_native_host_manifest(
            &directory.join(format!("{NATIVE_HOST_NAME}.json")),
            host,
            extension_id,
        )?;
    }
    Ok(())
}

fn ensure_pairing(directory: &Path) -> Result<(String, String), String> {
    if let Some(pairing) = current_pairing(directory) {
        return Ok(pairing);
    }

    spawn_daemon(directory)?;
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(pairing) = current_pairing(directory) {
            return Ok(pairing);
        }
        thread::sleep(Duration::from_millis(100));
    }

    Err("Gundad did not become available.".into())
}

fn current_pairing(directory: &Path) -> Option<(String, String)> {
    let daemon_endpoint = read_endpoint(directory).ok()?;
    if !reachable(&daemon_endpoint) {
        return None;
    }

    let endpoint = load_capture_endpoint(&capture_endpoint_path(directory))
        .ok()??
        .trim()
        .to_owned();
    let token = load_capture_token(&capture_token_path(directory)).ok()??;
    if !capture_reachable(&endpoint) {
        return None;
    }

    Some((endpoint, token.as_str().to_owned()))
}

fn reachable(endpoint: &str) -> bool {
    let Ok(mut addresses) = endpoint.to_socket_addrs() else {
        return false;
    };
    addresses.any(|address| TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).is_ok())
}

fn capture_reachable(endpoint: &str) -> bool {
    let Some(authority) = endpoint
        .strip_prefix("ws://")
        .or_else(|| endpoint.strip_prefix("wss://"))
        .and_then(|endpoint| endpoint.split('/').next())
    else {
        return false;
    };

    reachable(authority)
}

fn spawn_daemon(directory: &Path) -> Result<(), String> {
    let executable = env::current_exe().map_err(|_| "Could not find the Gunda executable.")?;
    let mut command = Command::new(executable);
    command
        .arg("--daemon")
        .arg("--data-dir")
        .arg(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    hide_window(&mut command);

    command
        .spawn()
        .map(|_| ())
        .map_err(|_| "Could not start Gundad.".into())
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::fs;
    use std::path::Path;

    #[cfg(target_os = "linux")]
    use tempfile::tempdir;

    #[cfg(target_os = "linux")]
    use super::{CHROME_EXTENSION_ID, install_chromium_manifests, shell_quote};
    #[cfg(any(windows, test))]
    use super::{WINDOWS_CHROMIUM_HOSTS, batch_script, windows_registration_arguments};

    #[cfg(target_os = "linux")]
    #[test]
    fn shell_quote_preserves_apostrophes() {
        assert_eq!(
            shell_quote("/tmp/Gunda's file"),
            "'/tmp/Gunda'\"'\"'s file'"
        );
    }

    #[test]
    fn windows_launcher_quotes_paths_and_escapes_percent() {
        assert_eq!(
            batch_script(
                r"C:\Program Files\Gunda\gunda-desktop.exe",
                r"C:\Users\A%B\Gunda",
            ),
            "@echo off\r\nsetlocal DisableDelayedExpansion\r\nchcp 65001 >nul 2>&1\r\n\"C:\\Program Files\\Gunda\\gunda-desktop.exe\" --native-host --data-dir \"C:\\Users\\A%%B\\Gunda\"\r\n"
        );
    }

    #[test]
    fn windows_registration_points_each_browser_at_the_manifest() {
        let manifest = Path::new(r"C:\Users\Gunda User\AppData\Local\gunda\com.gunda.capture.json");

        for browser in WINDOWS_CHROMIUM_HOSTS {
            let arguments = windows_registration_arguments(browser, manifest);

            let expected_key = format!(r"HKCU\{browser}\com.gunda.capture");

            assert_eq!(arguments[0], "add");
            assert_eq!(arguments[1], expected_key.as_str());
            assert_eq!(arguments[2], "/ve");
            assert_eq!(arguments[3], "/t");
            assert_eq!(arguments[4], "REG_SZ");
            assert_eq!(arguments[5], "/d");
            assert_eq!(arguments[6], manifest.as_os_str().to_owned());
            assert_eq!(arguments[7], "/f");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn chromium_manifest_uses_the_stable_extension_origin() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config");
        fs::create_dir_all(config.join("google-chrome")).unwrap();
        let host = directory.path().join("gunda-native-host");

        install_chromium_manifests(&config, &host, CHROME_EXTENSION_ID).unwrap();

        let path = config.join("google-chrome/NativeMessagingHosts/com.gunda.capture.json");
        let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            value["allowed_origins"][0],
            format!("chrome-extension://{CHROME_EXTENSION_ID}/")
        );
    }
}
