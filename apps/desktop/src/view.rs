use std::path::PathBuf;

use gunda_core::download::{DownloadId, DownloadJob, DownloadState, FailureKind};

#[derive(Clone)]
pub struct DownloadView {
    pub id: DownloadId,
    pub name: String,
    pub state: DownloadState,
    pub written: u64,
    pub total: Option<u64>,
    pub source: String,
    pub origin: &'static str,
    pub destination: PathBuf,
    pub output: Option<PathBuf>,
    pub error: Option<&'static str>,
}

impl DownloadView {
    pub fn from_job(job: &DownloadJob) -> Self {
        let output = (job.state() == DownloadState::Completed)
            .then(|| {
                job.resolved_destination()
                    .map(|d| d.final_path().to_owned())
            })
            .flatten();
        let name = job
            .resolved_destination()
            .and_then(|d| d.final_path().file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .or_else(|| job.destination().preferred_filename().map(str::to_owned))
            .unwrap_or_else(|| {
                if matches!(
                    job.state(),
                    DownloadState::Queued | DownloadState::Inspecting
                ) {
                    "Preparing download…".into()
                } else {
                    "Unnamed file".into()
                }
            });
        Self {
            id: job.id(),
            name,
            state: job.state(),
            written: job.progress().downloaded_bytes(),
            total: job.progress().total_bytes(),
            source: job.request().url().origin().ascii_serialization(),
            origin: match job.origin() {
                gunda_core::download::DownloadOrigin::Desktop => "Desktop",
                gunda_core::download::DownloadOrigin::Browser { .. } => "Browser",
                gunda_core::download::DownloadOrigin::Cli => "CLI",
            },
            destination: job.destination().directory().to_owned(),
            output,
            error: (job.state() == DownloadState::Failed)
                .then(|| {
                    job.last_failure()
                        .map(|failure| failure_message(failure.kind()))
                })
                .flatten(),
        }
    }

    pub fn can_resume(&self) -> bool {
        matches!(
            self.state,
            DownloadState::Paused | DownloadState::Interrupted
        )
    }

    pub fn progress(&self) -> Option<f32> {
        if self.state == DownloadState::Completed {
            return Some(1.0);
        }
        self.total
            .filter(|total| *total > 0)
            .map(|total| (self.written as f64 / total as f64).clamp(0.0, 1.0) as f32)
    }

    pub fn label(&self, active: bool) -> &'static str {
        if active && self.state == DownloadState::Queued {
            return "Preparing";
        }
        match self.state {
            DownloadState::Queued => "Waiting",
            DownloadState::Inspecting => "Preparing",
            DownloadState::Downloading => "Downloading",
            DownloadState::Paused => "Paused",
            DownloadState::Finalizing => "Finishing",
            DownloadState::Completed => "Completed",
            DownloadState::Failed => "Failed",
            DownloadState::Cancelled => "Cancelled",
            DownloadState::Interrupted => "Interrupted",
        }
    }
}

fn failure_message(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Network => "Connection problem. Check your internet connection.",
        FailureKind::Authentication => "The server requires authorization for this download.",
        FailureKind::RemoteRejected => "The server refused the download.",
        FailureKind::InvalidResponse => "The server returned an unexpected response.",
        FailureKind::UnsupportedResource => "This type of download is not supported.",
        FailureKind::PermissionDenied => "Gunda cannot save the file in this location.",
        FailureKind::DiskFull => "There is not enough space to save this file.",
        FailureKind::Integrity => "The downloaded file could not be verified.",
        FailureKind::Storage => "Gunda could not save the downloaded file.",
        FailureKind::Internal => "The download stopped unexpectedly.",
    }
}

pub fn bytes(value: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit < units.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{amount:.1} {}", units[unit])
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sample(state: DownloadState) -> DownloadView {
        DownloadView {
            id: DownloadId::new(1).unwrap(),
            name: "file.bin".into(),
            state,
            written: 25,
            total: Some(100),
            source: "https://example.com".into(),
            origin: "Desktop",
            destination: PathBuf::from("Downloads"),
            output: None,
            error: None,
        }
    }

    #[test]
    fn percentages_preserve_partial_progress_and_unknown_sizes() {
        let mut view = sample(DownloadState::Interrupted);
        assert_eq!(view.progress(), Some(0.25));
        view.total = None;
        assert_eq!(view.progress(), None);
        view.total = Some(0);
        assert_eq!(view.progress(), None);
        view.state = DownloadState::Completed;
        assert_eq!(view.progress(), Some(1.0));
    }

    #[test]
    fn only_interrupted_or_paused_jobs_offer_resume() {
        for state in [DownloadState::Paused, DownloadState::Interrupted] {
            assert!(sample(state).can_resume());
        }
        for state in [
            DownloadState::Completed,
            DownloadState::Cancelled,
            DownloadState::Failed,
        ] {
            assert!(!sample(state).can_resume());
        }
    }
}
