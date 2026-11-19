use thiserror::Error;

/// Failures that stop or degrade a recording.
///
/// Variants are categories rather than messages; the concrete cause is kept in
/// the source chain so the application can surface it through `anyhow`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RecordingError {
    /// A recording directory or file could not be read, written, or synchronized.
    #[error("recording storage operation failed")]
    Io(#[from] std::io::Error),
    /// The platform screen-capture backend could not run.
    #[error("platform screen capture failed")]
    Platform(#[source] PlatformError),
    /// The H.264 encoder could not run.
    #[error("video encoding failed")]
    Encoder(#[source] EncoderError),
    /// The recording holds no sealed segment, so there is nothing to finish.
    #[error("recording has no sealed segments")]
    NoSegments,
    /// Sealed segments could not be merged into the final video.
    #[error("segment merge failed")]
    Merge(#[source] MergeError),
}

/// Why the platform capture backend could not run.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PlatformError {
    /// This crate provides no capture backend for the current platform.
    #[error("screen capture is not supported on this platform")]
    Unsupported,
    /// The capture backend could not initialize or deliver a frame.
    #[error("screen capture failed")]
    Failed(#[source] CaptureFailure),
}

/// Failure reported by the platform capture backend.
///
/// Only the platform error code is kept, so callers can classify the failure
/// without this crate exposing platform types in its public surface.
#[derive(Debug, Error)]
#[error("capture failed with platform error 0x{code:X}")]
pub struct CaptureFailure {
    code: i32,
}

impl CaptureFailure {
    /// Creates a failure from a platform error code.
    #[must_use]
    pub const fn new(code: i32) -> Self {
        Self { code }
    }

    /// Returns the platform error code.
    #[must_use]
    pub const fn code(&self) -> i32 {
        self.code
    }
}

/// Why the H.264 encoder could not run.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EncoderError {
    /// This crate provides no encoder for the current platform.
    #[error("video encoding is not supported on this platform")]
    Unavailable,
}

/// Why sealed segments could not be merged.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MergeError {
    /// Merging is not implemented yet.
    #[error("segment merging is not implemented")]
    Unsupported,
}
