//! Client runtime state reported to the server.

use serde::{Deserialize, Serialize};

/// Operating system hosting the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Platform {
    /// Microsoft Windows.
    Windows,
    /// Apple macOS.
    Macos,
}

/// Stage of the client exam runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExecutionState {
    /// Connected and waiting for the start command.
    Ready,
    /// Capturing screen and reporting events.
    Recording,
    /// Stopping capture and merging the recording.
    Finalizing,
    /// The recording summary has been acknowledged.
    Finished,
}

/// Stage of the local recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RecordingState {
    /// No capture is running.
    Idle,
    /// Segments are being captured.
    Capturing,
    /// Capture has stopped and the recording is being merged.
    Finalizing,
    /// The merged recording is complete.
    Complete,
    /// Capture or merging failed.
    Failed,
}

/// Snapshot of the client runtime.
///
/// Only the documented `execution` / `recording` pairs are valid:
/// `ready/idle`, `recording/capturing`, `recording/failed`,
/// `finalizing/finalizing`, `finalizing/complete`, `finalizing/failed`,
/// `finished/complete`, and `finished/failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientState {
    /// Exam runtime stage.
    pub execution: ExecutionState,
    /// Recorder stage.
    pub recording: RecordingState,
}
