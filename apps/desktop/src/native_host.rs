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

pub fn install(directory: &Path) -> Result<(), Box<dyn Error>> {
    let executable = env::current_exe()?;
    let host = launcher(directory, &executable)?;

    #[cfg(target_os = "linux")]
    {
        let config = directories::BaseDirs::new()
            .ok_or("Could not find the browser configuration folder.")?
            .config_dir()
            .to_path_buf();
        install_chromium_manifests(&config, &host, CHROME_EXTENSION_ID)?;
    }

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

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(&host, fs::Permissions::from_mode(0o700))?;
    }

    Ok(host)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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
    Command::new(executable)
        .arg("--daemon")
        .arg("--data-dir")
        .arg(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|_| "Could not start Gundad.".into())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{CHROME_EXTENSION_ID, install_chromium_manifests, shell_quote};

    #[test]
    fn shell_quote_preserves_apostrophes() {
        assert_eq!(
            shell_quote("/tmp/Gunda's file"),
            "'/tmp/Gunda'\"'\"'s file'"
        );
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
