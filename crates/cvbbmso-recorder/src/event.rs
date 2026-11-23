use cvbbmso_protocol::SegmentMeta;

use crate::RecordingError;

/// Notifications emitted by a running recorder.
#[derive(Debug)]
#[non_exhaustive]
pub enum RecorderEvent {
    /// A segment was sealed and its metadata persisted.
    ///
    /// Segments restored by [`crate::Recorder::open`] are reported before any
    /// segment recorded by the current session.
    SegmentFinalized(SegmentMeta),
    /// The capture pipeline failed and stopped; recording does not resume on
    /// its own.
    Fault(RecordingError),
}
