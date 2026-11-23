//! Recorder operations the runtime drives.
//!
//! The trait is the test seam between the runtime and the platform recorder:
//! the real [`Recorder`] needs a display and an encoder, while the runtime's
//! command, retry, and reporting rules must be exercised without either.

use std::future::Future;

use cvbbmso_protocol::RecordingSummary;
use cvbbmso_recorder::{Recorder, RecordingError};

/// Lifecycle operations of one candidate recording.
pub(crate) trait Capture: Send + 'static {
    /// Starts or resumes capture; succeeds without effect while capturing.
    fn start(&mut self) -> impl Future<Output = Result<(), RecordingError>> + Send;

    /// Stops capture and seals the current segment.
    fn stop(&mut self) -> impl Future<Output = Result<(), RecordingError>> + Send;

    /// Stops capture, merges the sealed segments, and returns the summary.
    fn finish(&mut self) -> impl Future<Output = Result<RecordingSummary, RecordingError>> + Send;
}

impl Capture for Recorder {
    fn start(&mut self) -> impl Future<Output = Result<(), RecordingError>> + Send {
        Recorder::start(self)
    }

    fn stop(&mut self) -> impl Future<Output = Result<(), RecordingError>> + Send {
        Recorder::stop(self)
    }

    fn finish(&mut self) -> impl Future<Output = Result<RecordingSummary, RecordingError>> + Send {
        Recorder::finish(self)
    }
}
