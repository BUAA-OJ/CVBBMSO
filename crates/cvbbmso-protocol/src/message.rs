//! WebSocket messages exchanged over the exam connection.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{ClientState, ExamEvent, ExamPhase, Platform, RecordingPolicy};

/// First message sent by a client on a new connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    /// Persistent device identifier.
    pub device_id: Uuid,
    /// Identifier of the current client process instance.
    pub instance_id: Uuid,
    /// Operating system hosting the client.
    pub platform: Platform,
    /// Client state at connection time.
    pub state: ClientState,
}

/// Message sent by the client to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientMessage {
    /// Binds the connection to a device and process instance.
    Hello(ClientHello),
    /// Keep-alive message without a payload.
    Heartbeat,
    /// Current client state.
    Status {
        /// Client runtime snapshot.
        state: ClientState,
    },
    /// Reliable, ordered business event.
    Event {
        /// Per-exam sequence number, starting at one.
        seq: u64,
        /// Event payload.
        event: ExamEvent,
    },
    /// Final result of a server command.
    CommandResult {
        /// Command being answered.
        command_id: Uuid,
        /// Terminal status.
        status: CommandStatus,
        /// Error code when `status` is `failed`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
    },
}

/// Message sent by the server to the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServerMessage {
    /// Authoritative server snapshot after `hello`.
    Sync {
        /// Server clock at snapshot time.
        #[serde(with = "crate::utc::rfc3339")]
        server_time: OffsetDateTime,
        /// Phase derived from the server clock.
        phase: ExamPhase,
        /// Fixed exam start time.
        #[serde(with = "crate::utc::rfc3339")]
        start_at: OffsetDateTime,
        /// Fixed exam end time.
        #[serde(with = "crate::utc::rfc3339")]
        end_at: OffsetDateTime,
        /// Recording parameters for this exam.
        recording_policy: RecordingPolicy,
        /// Highest contiguous event sequence the server has persisted.
        ack_event_through: u64,
    },
    /// Start or end command.
    Command {
        /// Unique command identity; every command gets a fresh id.
        command_id: Uuid,
        /// Command name.
        name: CommandName,
        /// Effective time on the server timeline.
        #[serde(with = "crate::utc::rfc3339")]
        effective_at: OffsetDateTime,
    },
    /// Confirms that a `command_result` was processed.
    CommandAck {
        /// Command whose result was processed.
        command_id: Uuid,
    },
    /// Protocol or authorization error.
    Error {
        /// Stable error code from [`crate::error_code`].
        code: String,
        /// Display text for diagnostics.
        message: String,
    },
}

/// Name of a server command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandName {
    /// Start recording and enter the recording state.
    StartExam,
    /// Stop recording and finalize the manifest.
    EndExam,
}

/// Terminal status reported for a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandStatus {
    /// The command completed successfully.
    Completed,
    /// The command failed.
    Failed,
}
