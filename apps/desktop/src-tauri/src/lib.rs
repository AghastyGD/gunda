use std::path::PathBuf;
use std::sync::Mutex;

use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use url::Url;

#[derive(Default)]
struct DesktopState {
  destination: Mutex<Option<PathBuf>>,
}

#[tauri::command]
async fn choose_directory(
  app: AppHandle,
  state: State<'_, DesktopState>,
) -> Result<Option<String>, String> {
  let selected = tauri::async_runtime::spawn_blocking(move || {
    app.dialog().file().blocking_pick_folder()
  })
  .await
  .map_err(|_| "Could not open the folder picker.".to_owned())?;

  let Some(selected) = selected else {
    return Ok(None);
  };

  let path = selected
    .into_path()
    .map_err(|_| "The selected destination is not a local folder".to_owned())?;

  // Keep the native path; the string is only for presentation.
  let display_path = path.to_string_lossy().into_owned();

  let mut destination = state
    .destination
    .lock()
    .map_err(|_| "Could not update the dsetination".to_owned())?;

  *destination = Some(path);

  Ok(Some(display_path))
  
}

#[tauri::command]
fn validate_download_input(
  url: String,
  state: State<'_, DesktopState>,
) -> Result<(), String> {
  validate_http_url(&url).map_err(str::to_owned)?;

  let destination = state
    .destination
    .lock()
    .map_err(|_| "Could not read the destination.".to_owned())?;

  if destination.is_none() {
    return Err("Choose a destination folder.".to_owned());
  }

  Ok(())
}

fn validate_http_url(value: &str) -> Result<(), &'static str> {
  let url = Url::parse(value.trim())
    .map_err(|_| "Enter a valid URL.")?;

  if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
    return Err("Use an HTTP or HTTPS URL.");
  }

  if !url.username().is_empty() || url.password().is_some() {
    return Err("URLs containing a username or password are not supported.");
  }

  Ok(())

}

pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_dialog::init())
    .manage(DesktopState::default())
    .invoke_handler(tauri::generate_handler!{
      choose_directory,
      validate_download_input,
    })
    .run(tauri::generate_context!())
    .expect("failed to run Gunda")
}

#[cfg(test)]
mod tests {
    use super::validate_http_url;

    #[test]
    fn accepts_http_and_https_urls() {
        for value in [
            "http://localhost/file.bin",
            "https://example.com/file.zip",
            " https://example.com/file.zip?token=value ",
        ] {
            assert!(validate_http_url(value).is_ok());
        }
    }

    #[test]
    fn rejects_invalid_urls_and_unsupported_schemes() {
        for value in [
            "",
            "not a URL",
            "file:///tmp/file.bin",
            "ftp://example.com/file.bin",
            "javascript:alert(1)",
        ] {
            assert!(validate_http_url(value).is_err());
        }
    }

    #[test]
    fn rejects_embedded_credentials() {
        for value in [
            "https://user@example.com/file.bin",
            "https://user:secret@example.com/file.bin",
        ] {
            assert!(validate_http_url(value).is_err());
        }
    }
}