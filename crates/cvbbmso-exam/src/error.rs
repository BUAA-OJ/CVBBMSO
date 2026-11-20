use cvbbmso_recorder::RecordingError;
use thiserror::Error;
use tokio_tungstenite::tungstenite;

/// Failures of the exam runtime.
///
/// Variants are categories; the underlying cause, when one exists, stays in the
/// source chain so the application can report it through `anyhow`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExamError {
    /// The monitor HTTP API could not be reached or answered unexpectedly.
    #[error("monitor HTTP request failed")]
    Http(#[source] Option<reqwest::Error>),
    /// The monitor WebSocket could not be established or broke the protocol.
    #[error("monitor WebSocket connection failed")]
    WebSocket(#[source] Option<tungstenite::Error>),
    /// The Accoding session is missing, expired, or anonymous.
    #[error("Accoding session is missing or expired")]
    Auth,
    /// The authenticated student is not enrolled in the exam.
    #[error("student is not enrolled in the exam")]
    NotEnrolled,
    /// The recorder could not be opened or driven.
    #[error("recording failed")]
    Recording(#[source] RecordingError),
    /// The local runtime data could not be read or written.
    #[error("local exam storage failed")]
    Storage(#[source] std::io::Error),
    /// The runtime has stopped and no longer accepts requests.
    #[error("exam runtime is closed")]
    Closed,
}
