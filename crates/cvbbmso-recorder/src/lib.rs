//! Primary-display screen recording for the cvbbmso exam client.
//!
//! The crate owns the recording lifecycle defined in
//! `docs/design/crates/cvbbmso-recorder.md`: it opens the candidate recording
//! directory, restores the segments sealed by an interrupted session, records
//! the primary display, seals every segment with its SHA-256, and merges the
//! recording into a final video. Platform capture, encoding, and container
//! details stay private to this crate.

#![allow(clippy::module_name_repetitions)]

mod capture;
mod container;
mod encode;
mod error;
mod event;
mod recorder;
mod store;

pub use error::RecordingError;
pub use event::RecorderEvent;
pub use recorder::Recorder;
