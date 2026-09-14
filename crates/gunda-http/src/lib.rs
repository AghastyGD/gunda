mod body;
mod client;
mod error;
mod executor;
mod finalize;
mod partial;

pub use body::HttpBody;
pub use client::{HttpClient, HttpInspection};
pub use error::HttpError;
pub use executor::{HttpExecutor, HttpPreparedTransfer, HttpStagedTransfer};
pub use finalize::{FinalizeError, FinalizeFailure, FinalizedDownload, finalize_download};
pub use partial::{
    FileOperation, PartialDownload, PartialDownloadError, download_to_partial, partial_path,
};
