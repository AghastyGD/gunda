use crate::download::{
    DownloadFailure, DownloadId, DownloadProgress, DownloadState, ResolvedDestination,
};

/// Runtime notification emitted after an application-level change is confirmed.
#[derive(Clone, PartialEq, Eq)]
pub enum DownloadEvent {
    Created {
        id: DownloadId,
    },

    StateChanged {
        id: DownloadId,
        previous: DownloadState,
        current: DownloadState,
    },

    ProgressChanged {
        id: DownloadId,
        progress: DownloadProgress,
    },

    Completed {
        id: DownloadId,
        destination: ResolvedDestination,
    },

    Failed {
        id: DownloadId,
        failure: DownloadFailure,
    },

    Removed {
        id: DownloadId,
    },
}

/// Non-sensitive classification of a download event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadEventKind {
    Created,
    StateChanged,
    ProgressChanged,
    Completed,
    Failed,
    Removed,
}

impl DownloadEvent {
    /// Returns a safe classification of this event.
    #[must_use]
    pub const fn kind(&self) -> DownloadEventKind {
        match self {
            Self::Created { .. } => DownloadEventKind::Created,
            Self::StateChanged { .. } => DownloadEventKind::StateChanged,
            Self::ProgressChanged { .. } => DownloadEventKind::ProgressChanged,
            Self::Completed { .. } => DownloadEventKind::Completed,
            Self::Failed { .. } => DownloadEventKind::Failed,
            Self::Removed { .. } => DownloadEventKind::Removed,
        }
    }

    /// Returns the download affected by this event.
    #[must_use]
    pub const fn download_id(&self) -> DownloadId {
        match self {
            Self::Created { id }
            | Self::StateChanged { id, .. }
            | Self::ProgressChanged { id, .. }
            | Self::Completed { id, .. }
            | Self::Failed { id, .. }
            | Self::Removed { id } => *id,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{DownloadEvent, DownloadEventKind};
    use crate::download::{
        DownloadFailure, DownloadId, DownloadProgress, DownloadState, FailureKind,
        ResolvedDestination,
    };

    fn download_id() -> DownloadId {
        DownloadId::new(26).expect("test ID must be valid")
    }

    #[test]
    fn event_kind_and_download_id_are_reported_for_every_event() {
        let id = download_id();

        let events = [
            (DownloadEvent::Created { id }, DownloadEventKind::Created),
            (
                DownloadEvent::StateChanged {
                    id,
                    previous: DownloadState::Queued,
                    current: DownloadState::Inspecting,
                },
                DownloadEventKind::StateChanged,
            ),
            (
                DownloadEvent::ProgressChanged {
                    id,
                    progress: DownloadProgress::default(),
                },
                DownloadEventKind::ProgressChanged,
            ),
            (
                DownloadEvent::Completed {
                    id,
                    destination: ResolvedDestination::new(
                        PathBuf::from("downloads").join("file.iso"),
                    ),
                },
                DownloadEventKind::Completed,
            ),
            (
                DownloadEvent::Failed {
                    id,
                    failure: DownloadFailure::new(FailureKind::Network, "connection closed", true),
                },
                DownloadEventKind::Failed,
            ),
            (DownloadEvent::Removed { id }, DownloadEventKind::Removed),
        ];

        for (event, expected_kind) in events {
            assert_eq!(event.kind(), expected_kind);
            assert_eq!(event.download_id(), id);
        }
    }

    #[test]
    fn completed_event_preserves_resolved_destination() {
        let id = download_id();
        let event = DownloadEvent::Completed {
            id,
            destination: ResolvedDestination::new(PathBuf::from("downloads").join("file.iso")),
        };

        let DownloadEvent::Completed {
            id: event_id,
            destination,
        } = event
        else {
            panic!("event must remain Completed");
        };

        assert_eq!(event_id, id);
        assert_eq!(
            destination.final_path(),
            Path::new("downloads").join("file.iso")
        )
    }
}
