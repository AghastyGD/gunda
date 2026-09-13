mod body;
mod client;
mod error;
mod partial;

pub use body::HttpBody;
pub use client::{HttpClient, HttpInspection};
pub use error::HttpError;
pub use partial::{
    FileOperation, PartialDownload, PartialDownloadError, download_to_partial, partial_path,
};
