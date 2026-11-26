//! Wire types for the cvbbmso HTTP and WebSocket protocol.
//!
//! This crate is the only source of the wire schema: DTOs, closed enums,
//! validated newtypes, and error-code constants. It performs no I/O and holds
//! no business logic; invariants are enforced by constructors and
//! deserializers.
//!
//! Field names and enum values are `snake_case`, error codes are uppercase
//! `SNAKE_CASE`, timestamps are RFC 3339 in UTC, UUIDs use the standard textual
//! form, and digests are 64 lowercase hexadecimal characters.

#![forbid(unsafe_code)]
// Public items are re-exported from the crate root, so a module prefix would
// only repeat the type name at the definition site.
#![allow(clippy::module_name_repetitions)]

mod digest;
mod error;
mod event;
mod exam;
mod incident;
mod message;
mod recording;
mod state;
mod utc;

pub mod admin;
pub mod error_code;

pub use digest::Sha256Digest;
pub use error::ProtocolError;
pub use event::ExamEvent;
pub use exam::{AccodingUser, ExamListResponse, ExamPhase, ExamSummary};
pub use incident::{ClientIncident, IncidentCode};
pub use message::{ClientHello, ClientMessage, CommandName, CommandStatus, ServerMessage};
pub use recording::{RecordingPolicy, RecordingSummary, SegmentMeta};
pub use state::{ClientState, ExecutionState, Platform, RecordingState};
