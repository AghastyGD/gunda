use std::error::Error;
use std::fmt;

/// Failure while reading a segmented-stream manifest.
///
/// This deliberately carries no URLs, keys, or manifest text: manifests can
/// contain account-bound tokens and DRM data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestError {
    /// The text is not a manifest of the requested kind.
    NotAManifest,
    /// Structural damage: a required field is missing or contradictory.
    Malformed,
    /// The text mixes master and media playlist forms.
    AmbiguousPlaylist,
    /// A byterange has no resolvable offset or a zero length.
    InvalidByteRange,
    /// A numeric attribute could not be represented.
    InvalidNumber,
    /// The selected track is no longer present in the manifest.
    SelectionUnavailable,
    /// A recognized feature that this engine refuses to guess about.
    Unsupported(UnsupportedFeature),
}

/// Recognized manifest features that are not implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedFeature {
    /// Encrypted HLS segments (`EXT-X-KEY` with a non-`NONE` method).
    EncryptedSegments,
    /// Inline `data:` segments, which cannot be fetched as resources.
    DataUriSegments,
    /// DASH single-file addressing that requires a segment index (`sidx`).
    SegmentIndex,
    /// DASH separate audio and video tracks, which require muxing.
    SeparateTracks,
    /// DASH multi-period presentations, which require timeline stitching.
    MultiPeriod,
    /// A live or otherwise unfinished presentation.
    IncompletePresentation,
}

impl fmt::Display for UnsupportedFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EncryptedSegments => f.write_str("encrypted segments are not supported"),
            Self::DataUriSegments => f.write_str("inline data segments are not supported"),
            Self::SegmentIndex => {
                f.write_str("segment-index addressing requires a range index that is not supported")
            }
            Self::SeparateTracks => f.write_str("separate audio and video tracks require muxing"),
            Self::MultiPeriod => f.write_str("multi-period presentations are not supported"),
            Self::IncompletePresentation => {
                f.write_str("the presentation is not finished and cannot be captured")
            }
        }
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAManifest => f.write_str("the resource is not a segmented-stream manifest"),
            Self::Malformed => f.write_str("the manifest is malformed"),
            Self::AmbiguousPlaylist => {
                f.write_str("the playlist mixes master and media playlist entries")
            }
            Self::InvalidByteRange => f.write_str("the manifest declares an invalid byte range"),
            Self::InvalidNumber => f.write_str("the manifest declares an invalid number"),
            Self::SelectionUnavailable => {
                f.write_str("the selected quality is no longer available")
            }
            Self::Unsupported(feature) => write!(f, "{feature}"),
        }
    }
}

impl Error for ManifestError {}

impl From<UnsupportedFeature> for ManifestError {
    fn from(feature: UnsupportedFeature) -> Self {
        Self::Unsupported(feature)
    }
}
