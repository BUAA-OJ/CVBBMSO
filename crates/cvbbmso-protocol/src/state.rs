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
    /// The runtime cannot continue until it recovers.
    Error,
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

/// Result of the client network inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NetworkState {
    /// No proxy was detected and the server is reachable.
    Clear,
    /// A system proxy, PAC, or proxy environment variable was detected.
    ProxyDetected,
    /// The server could not be reached.
    Unreachable,
    /// The inspection could not complete.
    Unknown,
}

/// Snapshot of the client runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientState {
    /// Exam runtime stage.
    pub execution: ExecutionState,
    /// Recorder stage.
    pub recording: RecordingState,
    /// Latest network inspection result.
    pub network: NetworkState,
}
