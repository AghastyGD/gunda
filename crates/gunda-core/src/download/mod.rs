mod destination;
mod failure;
mod id;
mod origin;
mod progress;
mod request;
mod state;

pub use destination::{DownloadDestination, FileConflictPolicy, ResolvedDestination};
pub use failure::{DownloadFailure, FailureKind};
pub use id::{DownloadId, InvalidDownloadId};
pub use origin::DownloadOrigin;
pub use progress::{DownloadProgress, InvalidDownloadProgress};
pub use request::{HeaderSensitivity, RequestContext, RequestHeader};
pub use state::{DownloadState, InvalidStateTransition};
