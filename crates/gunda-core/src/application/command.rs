use crate::download::{DownloadId, NewDownload};

/// Intent submitted by an application client.
#[derive(Clone, PartialEq, Eq)]
pub enum DownloadCommand {
    Create(NewDownload),

    Pause(DownloadId),

    Resume(DownloadId),

    Cancel(DownloadId),

    Retry(DownloadId),

    Remove {
        id: DownloadId,
        delete_partial_data: bool,
    },
}

/// Non-sensitive classification of a download command.
///
/// This type can be used in diagnostic output without exposing request URLs,
/// headers, destination paths, or browser context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadCommandKind {
    Create,
    Pause,
    Resume,
    Cancel,
    Retry,
    Remove,
}

impl DownloadCommand {
    /// Returns a safe classification of this command.
    #[must_use]
    pub const fn kind(&self) -> DownloadCommandKind {
        match self {
            Self::Create(_) => DownloadCommandKind::Create,
            Self::Pause(_) => DownloadCommandKind::Pause,
            Self::Resume(_) => DownloadCommandKind::Resume,
            Self::Cancel(_) => DownloadCommandKind::Cancel,
            Self::Retry(_) => DownloadCommandKind::Retry,
            Self::Remove { .. } => DownloadCommandKind::Remove,
        }
    }

    /// Returns the target job ID when the command addresses an existing job.
    ///
    /// `Create` has no target because storage has not assigned an ID yet.
    #[must_use]
    pub fn target_id(&self) -> Option<DownloadId> {
        match self {
            Self::Create(_) => None,
            Self::Pause(id) | Self::Resume(id) | Self::Cancel(id) | Self::Retry(id) => Some(*id),
            Self::Remove { id, .. } => Some(*id),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use url::Url;

    use super::{DownloadCommand, DownloadCommandKind};
    use crate::download::{
        DownloadDestination, DownloadId, DownloadOrigin, FileConflictPolicy, NewDownload,
        RequestContext,
    };

    fn download_id() -> DownloadId {
        DownloadId::new(26).expect("test ID must be valid")
    }

    fn new_download() -> NewDownload {
        NewDownload::new(
            RequestContext::new(
                Url::parse("https://example.com/file.iso").expect("test URL must be valid"),
                Vec::new(),
            ),
            DownloadDestination::new(
                PathBuf::from("downloads"),
                Some("file.iso".to_owned()),
                FileConflictPolicy::Rename,
            ),
            DownloadOrigin::Desktop,
        )
    }

    #[test]
    fn command_kind_and_target_are_reported_without_exposing_payload() {
        let id = download_id();

        let cases = [
            (
                DownloadCommand::Pause(id),
                DownloadCommandKind::Pause,
                Some(id),
            ),
            (
                DownloadCommand::Resume(id),
                DownloadCommandKind::Resume,
                Some(id),
            ),
            (
                DownloadCommand::Cancel(id),
                DownloadCommandKind::Cancel,
                Some(id),
            ),
            (
                DownloadCommand::Retry(id),
                DownloadCommandKind::Retry,
                Some(id),
            ),
            (
                DownloadCommand::Remove {
                    id,
                    delete_partial_data: true,
                },
                DownloadCommandKind::Remove,
                Some(id),
            ),
        ];

        for (command, expected_kind, expected_id) in cases {
            assert_eq!(command.kind(), expected_kind);
            assert_eq!(command.target_id(), expected_id);
        }
    }

    #[test]
    fn create_command_has_not_target_id() {
        let command = DownloadCommand::Create(new_download());

        assert_eq!(command.kind(), DownloadCommandKind::Create);
        assert_eq!(command.target_id(), None)
    }
}
