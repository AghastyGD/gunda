//! Segmented media engines for Gunda.
//!
//! This crate turns a segmented-stream manifest (HLS or MPEG-DASH) into an
//! ordered download plan and executes it through the shared
//! [`gunda_core::application::DownloadExecutor`] contract, so segmented
//! downloads reuse the same lifecycle, progress, cancellation, staging and
//! finalization machinery as plain file downloads.

mod error;
mod xml;

pub mod dash;
pub mod hls;

pub use dash::TrackKind;
pub use error::{ManifestError, UnsupportedFeature};

/// Maximum number of segments accepted from one manifest.
///
/// A manifest is remote input, so the segment count is bounded before any
/// per-segment work (or memory) is committed to it.
pub(crate) const MAX_SEGMENTS: usize = 200_000;
