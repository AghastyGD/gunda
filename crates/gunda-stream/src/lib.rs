//! Segmented media engines for Gunda.
//!
//! This crate turns a segmented-stream manifest (HLS or MPEG-DASH) into an
//! ordered download plan and executes it through the shared
//! [`gunda_core::application::DownloadExecutor`] contract, so segmented
//! downloads reuse the same lifecycle, progress, cancellation, staging and
//! finalization machinery as plain file downloads.

mod container;
mod error;
mod executor;
mod presentation;
mod resume;
mod selection;
mod ts;
mod xml;

pub mod dash;
pub mod hls;
pub mod mux;
pub mod plan;

pub use container::{Container, SNIFF_BYTES, detect, sniff};
pub use dash::TrackKind;
pub use error::{ManifestError, UnsupportedFeature};
pub use executor::{
    StreamExecutor, StreamPreparedTransfer, StreamRestartResetError, StreamStagedTransfer,
    output_filename,
};
pub use mux::{MuxError, mux_mp4};
pub use plan::{
    HlsOutcome, StreamKind, StreamResource, TrackPlan, plan_dash, plan_hls, substitute_template,
};
pub use presentation::{Presentation, PresentationTrack, TrackSize, inspect};
pub use resume::{
    FINGERPRINT_LEN, InvalidStreamResumeState, PlanFingerprint, StreamResumeState,
    StreamResumeStore, TrackCheckpoint,
};

/// Maximum number of segments accepted from one manifest.
///
/// A manifest is remote input, so the segment count is bounded before any
/// per-segment work (or memory) is committed to it.
pub(crate) const MAX_SEGMENTS: usize = 200_000;
