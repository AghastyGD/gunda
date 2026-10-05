mod body;
mod client;
mod destination;
mod error;
mod executor;
mod finalize;
mod partial;
mod range;
mod recovery;
mod validator;

pub use body::HttpBody;
pub use client::{HttpClient, HttpInspection};
pub use error::HttpError;
pub use executor::{HttpExecutor, HttpPreparedTransfer, HttpStagedTransfer};
pub use finalize::{FinalizeError, FinalizeFailure, FinalizedDownload, finalize_download};
pub use partial::{
    FileOperation, PartialDownload, PartialDownloadError, download_to_partial, partial_path,
};
pub use range::{ContentRange, HttpRangeBody};
pub use recovery::{
    CheckpointComparison, HttpRecoveryInspection, LocalFileState, RecoveryInspectionError,
    inspect_local_recovery,
};
pub use validator::{InvalidStrongEntityTag, StrongEntityTag};
