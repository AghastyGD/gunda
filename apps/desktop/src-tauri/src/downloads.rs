use std::sync::Mutex;

use gunda_core::application::{
    DownloadCancellation, DownloadEvent, DownloadManagerError,
};
use gunda_core::download::{
    DownloadDestination, DownloadId, DownloadJob, DownloadOrigin,
    DownloadState, FileConflictPolicy, NewDownload, RequestContext,
};
use serde::Serialize;
use tauri::State;
use tauri::ipc::Channel;

use super::DesktopState;

pub(crate) struct ActiveDownload {
    id: DownloadId,
    cancellation: DownloadCancellation,
}

struct ActiveGuard<'a> {
    slot: &'a Mutex<Option<ActiveDownload>>
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.slot.lock() {
            *active = None;
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct DownloadView {
    id: String,
    name: String,
    state: String,
    written_bytes: String,
    total_bytes: Option<String>,
    output_path: Option<String>,
    error: Option<String>,
}

impl DownloadView {
    fn from_job(job: &DownloadJob) -> Self {
        let name = job
            .resolved_destination()
            .and_then(|destination| destination.final_path().file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .or_else(|| job.destination().preferred_filename().map(str::to_owned))
            .unwrap_or_else(|| format!("Download #{}", job.id().value()));

        let output_path = if job.state() == DownloadState::Completed {
            job.resolved_destination()
                .map(|destination| {
                    destination.final_path().to_string_lossy().into_owned()
                })
        } else {
            None
        };

        Self {
            id: job.id().value().to_string(),
            name,
            state: format!("{:?}", job.state()).to_ascii_lowercase(),
            written_bytes: job.progress().downloaded_bytes().to_string(),
            total_bytes: job.progress().total_bytes().map(|total| total.to_string()),
            output_path,
            error: if job.state() == DownloadState::Failed {
                job.last_failure()
                    .map(|failure| format!("Download failed ({:?}).", failure.kind()))
            } else {
                None
            },
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum DownloadUpdate {
    Started {
        job: DownloadView,
    },
    Progress {
        id: String,
        written_bytes: String,
        total_bytes: Option<String>,
    }
}

#[derive(Serialize)]
pub(crate) struct ExecutionResponse {
    job: DownloadView,
    notice: Option<String>,
}

#[tauri::command]
pub(crate) async fn list_downloads(
    state: State<'_, DesktopState>,
) -> Result<Vec<DownloadView>, String> {
    let manager = state
        .manager
        .try_lock()
        .map_err(|_| "A download is currently running.".to_owned())?;

    Ok(manager.jobs().map(DownloadView::from_job).collect())
}

#[tauri::command]
pub(crate) async fn start_download(
    url: String,
    updates: Channel<DownloadUpdate>,
    state: State<'_, DesktopState>,
) -> Result<ExecutionResponse, String> {
    let url = super::validate_http_url(&url).map_err(str::to_owned)?;

    let directory = state
        .destination
        .lock()
        .map_err(|_| "Could not read the destination.".to_owned())?
        .clone()
        .ok_or_else(|| "Choose a destination folder.".to_owned())?;

    let mut manager = state
        .manager
        .try_lock()
        .map_err(|_| "Another download is already running.".to_owned())?;

    let created = manager
        .create(NewDownload::new(
            RequestContext::new(url, Vec::new()), 
            DownloadDestination::new(
                directory,
                None, 
                FileConflictPolicy::Rename,
            ), 
            DownloadOrigin::Desktop,
        ))
        .await
        .map_err(|_| "Could not sabe the new download.".to_owned())?;

    let id = created.download_id();
    let cancellation = DownloadCancellation::new();

    {
        let mut active = state
            .active
            .lock()
            .map_err(|_| "Could not register the active download.".to_owned())?;

        *active = Some(ActiveDownload {
            id,
            cancellation: cancellation.clone(),
        });
    }

    let _active_guard = ActiveGuard {
        slot: &state.active,
    };

    let initial = manager
        .job(id)
        .map(DownloadView::from_job)
        .ok_or_else(|| "The created download is unavailable.".to_owned())?;

    if updates.send(DownloadUpdate::Started { job: initial }).is_err() {
        manager
            .cancel(id)
            .await
            .map_err(|_| "Could not confirm cancellation after losing the UI connection.".to_owned())?;

        return Err("The download UI is no longer available.".to_owned());
    }

    let observer_cancellation = cancellation.clone();

    let result = manager
        .execute_controlled(
            id,
            &state.executor,
            cancellation,
            |event| {
                if let DownloadEvent::ProgressChanged { id, progress } = event {
                    let update = DownloadUpdate::Progress { 
                        id: id.value().to_string(),
                        written_bytes: progress.downloaded_bytes().to_string(),
                        total_bytes: progress.total_bytes().map(|total| total.to_string()), 
                    };

                    if updates.send(update).is_err() {
                        observer_cancellation.request();
                    }
                }
            },
        )
        .await;

    let mut view = manager
        .job(id)
        .map(DownloadView::from_job)
        .ok_or_else(|| "The executed download is unavailable.".to_owned())?;

    let notice = match result {
        Ok(report) => {
            if report.cleanup_pending {
                Some("The file was saved, but its staging link could not be removed.".to_owned())
            } else if matches!(
                report.event,
                DownloadEvent::StateChanged {
                    current: DownloadState::Cancelled,
                    ..
                }
            ) {
                Some("Download cancelled. Any existing partial file was kept.".to_owned())
            } else {
                None
            }
        }

        Err(DownloadManagerError::CompletionNotPersisted { 
            destination,
            ..
        }) => {
            view.output_path = Some(
                destination.final_path().to_string_lossy().into_owned(),
            );

            Some(
                "The file was saved, but completion could not be recorded. \
                Check the output before starting another download."
                    .to_owned(),
            )
        }

        Err(error) => Some(error.to_string()),
    };

    Ok(ExecutionResponse { 
        job: view, 
        notice,
    })

}

#[tauri::command]
pub(crate) fn cancel_download(
    id: String,
    state: State<'_, DesktopState>,
) -> Result<bool, String> {
    let value = id 
        .parse::<i64>()
        .map_err(|_| "Invalid download ID.".to_owned())?;

    let id = DownloadId::new(value)
        .map_err(|_| "Invalid donwload ID".to_owned())?;

    let active = state
        .active
        .lock()
        .map_err(|_| "Could not access the active download.".to_owned())?;

    let Some(active) = active.as_ref().filter(|active| active.id == id) else {
        return Ok(false);
    };

    active.cancellation.request();
    Ok(true)
}
