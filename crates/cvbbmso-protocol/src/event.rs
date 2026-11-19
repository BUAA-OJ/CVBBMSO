//! Reliable business events reported by the client.

use serde::{Deserialize, Serialize};

use crate::{ClientIncident, RecordingManifest, SegmentMeta};

/// A reliable client event carried by a `ClientMessage::Event`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExamEvent {
    /// One segment has been sealed and hashed.
    SegmentFinalized(SegmentMeta),
    /// The recording has been merged and hashed.
    RecordingFinalized(RecordingManifest),
    /// A client incident occurred.
    Incident(ClientIncident),
}
