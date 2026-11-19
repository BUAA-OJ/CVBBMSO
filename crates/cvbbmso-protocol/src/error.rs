//! Validation failures for wire values.

use thiserror::Error;

/// A wire value violates a documented invariant.
///
/// Variants name the failed category; the message is for diagnostics only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// A SHA-256 digest is not 64 lowercase hexadecimal characters.
    #[error("sha-256 digest must be 64 lowercase hexadecimal characters")]
    InvalidDigest,
    /// An Accoding user id is not a positive integer.
    #[error("Accoding user id must be positive")]
    InvalidUserId,
    /// A recording policy value is zero.
    #[error("recording policy values must be greater than zero")]
    InvalidRecordingPolicy,
    /// A time range ends before it starts.
    #[error("start_at must not be later than end_at")]
    InvalidTimeRange,
    /// A segment index is zero; indices start at one.
    #[error("segment index must start at 1")]
    InvalidSegmentIndex,
    /// A manifest is not ordered by strictly increasing segment index.
    #[error("segments must be ordered by strictly increasing index")]
    InvalidSegmentOrder,
}
