//! Stable error codes shared by HTTP errors, WebSocket `error` messages, and
//! `command_result` messages.
//!
//! The codes are a closed set for one protocol version. HTTP status mapping
//! belongs to the server crate.

/// Request parameters, path, or message format are invalid.
pub const INVALID_REQUEST: &str = "INVALID_REQUEST";
/// The Accoding cookie is missing, expired, or anonymous.
pub const ACCODING_AUTH_EXPIRED: &str = "ACCODING_AUTH_EXPIRED";
/// The authenticated user is not enrolled in the exam.
pub const NOT_ENROLLED: &str = "NOT_ENROLLED";
/// The requested exam does not exist.
pub const EXAM_NOT_FOUND: &str = "EXAM_NOT_FOUND";
/// Accoding identity verification is temporarily unavailable.
pub const ACCODING_UNAVAILABLE: &str = "ACCODING_UNAVAILABLE";
/// An internal server failure occurred.
pub const INTERNAL_ERROR: &str = "INTERNAL_ERROR";
/// The message order, type, or field does not match the protocol.
pub const PROTOCOL_ERROR: &str = "PROTOCOL_ERROR";
/// The client state cannot execute the command.
pub const COMMAND_STATE_CONFLICT: &str = "COMMAND_STATE_CONFLICT";
/// Starting or resuming recording failed.
pub const RECORDING_START_FAILED: &str = "RECORDING_START_FAILED";
/// Stopping, merging, or hashing the recording failed.
pub const RECORDING_FINISH_FAILED: &str = "RECORDING_FINISH_FAILED";
