mod destination;
mod failure;
mod id;
mod job;
mod origin;
mod progress;
mod request;
mod resource;
mod state;

pub use destination::{DownloadDestination, FileConflictPolicy, ResolvedDestination};
pub use failure::{DownloadFailure, FailureKind};
pub use id::{DownloadId, InvalidDownloadId};
pub use job::{DownloadJob, NewDownload};
pub use origin::DownloadOrigin;
pub use progress::{DownloadProgress, InvalidDownloadProgress};
pub use request::{HeaderSensitivity, RequestContext, RequestHeader};
pub use resource::{ResourceDescriptor, ResourceKind};
pub use state::{DownloadState, InvalidStateTransition};
