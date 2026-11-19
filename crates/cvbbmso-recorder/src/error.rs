use thiserror::Error;

/// Failures that stop or degrade a recording.
///
/// This is the crate's only public error type: platform, encoder, and merge
/// failures are categories of it rather than separate enums, as required by the
/// workspace rules. Variants carry the underlying cause when one exists, so the
/// application can print the whole chain through `anyhow`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RecordingError {
    /// A recording directory or file could not be read, written, or synchronized.
    #[error("recording storage operation failed")]
    Io(#[from] std::io::Error),
    /// The platform screen-capture backend could not run.
    ///
    /// Carries the backend failure when one was reported; `None` means this crate
    /// provides no capture backend for the current platform.
    #[error("platform screen capture failed")]
    Platform(#[source] Option<std::io::Error>),
    /// The H.264 encoder could not run.
    #[error("video encoding failed")]
    Encoder,
    /// The recording holds no sealed segment, so there is nothing to finish.
    #[error("recording has no sealed segments")]
    NoSegments,
    /// Sealed segments could not be merged into the final video.
    #[error("segment merge failed")]
    Merge,
}
