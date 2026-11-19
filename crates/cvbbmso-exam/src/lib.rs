//! Client exam runtime for cvbbmso.
//!
//! The crate runs one candidate through one exam as defined in
//! `docs/design/crates/cvbbmso-exam.md`: it keeps the monitor WebSocket alive,
//! executes `start_exam` / `end_exam` commands by driving the recorder, and
//! reports reliable events through a durable local queue that survives
//! disconnects and process restarts.

#![forbid(unsafe_code)]

mod error;
// The journal and the monitor endpoints are wired into the runtime by the
// session layer that follows in the same pull request; until then only their
// tests reach them.
#[allow(dead_code)]
mod journal;
#[allow(dead_code)]
mod monitor;

pub use error::ExamError;
