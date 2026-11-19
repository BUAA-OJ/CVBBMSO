//! Primary-display screen recording for the cvbbmso exam client.
//!
//! The crate owns the recording lifecycle defined in
//! `docs/design/crates/cvbbmso-recorder.md`: it opens the candidate recording
//! directory, restores the segments sealed by an interrupted session, records
//! the primary display, seals every segment with its SHA-256, and merges the
//! recording into a final video. Platform capture, encoding, and container
//! details stay private to this crate.
//!
//! Storage and recovery are implemented. The Windows encoder produces framed
//! H.264 samples, and finishing a non-empty recording merges its sealed segments
//! into `final.mp4`. [`Recorder::start`] still reports
//! [`RecordingError::Platform`] until the capture pipeline is wired to the
//! recorder.

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
