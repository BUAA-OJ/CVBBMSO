//! H.264 encoding of captured frames.

// `Recorder::start` reaches this module once the capture pipeline is wired; the
// platform probe exercises it until then.
#![allow(dead_code)]

#[cfg(windows)]
mod windows;

#[cfg(windows)]
#[allow(unused_imports)] // Wired into Recorder::start with the encoding milestone.
pub(crate) use windows::available_encoders;

#[cfg(not(windows))]
#[allow(unused_imports)] // Wired into Recorder::start with the encoding milestone.
pub(crate) use unsupported::available_encoders;

#[cfg(not(windows))]
mod unsupported {
    use crate::RecordingError;

    /// Reports the encoders Media Foundation offers for the recording policy.
    ///
    /// # Errors
    ///
    /// Always returns [`RecordingError::Encoder`]: this crate has no encoder for
    /// the current platform.
    pub(crate) fn available_encoders() -> Result<Vec<String>, RecordingError> {
        Err(RecordingError::Encoder)
    }
}
