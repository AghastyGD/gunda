mod failure;
mod id;
mod progress;
mod state;

pub use failure::{DownloadFailure, FailureKind};
pub use id::{DownloadId, InvalidDownloadId};
pub use progress::{DownloadProgress, InvalidDownloadProgress};
pub use state::{DownloadState, InvalidStateTransition};
