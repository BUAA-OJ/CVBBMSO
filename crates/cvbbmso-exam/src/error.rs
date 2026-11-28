use cvbbmso_recorder::RecordingError;
use thiserror::Error;
use tokio_tungstenite::tungstenite;

/// Failures of the monitor client and exam runtime.
///
/// Variants are categories; the underlying cause, when one exists, stays in the
/// source chain so the application can report it through `anyhow`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExamError {
    /// The monitor HTTP API could not be reached or answered unexpectedly.
    #[error("monitor HTTP request failed")]
    Http(#[source] Option<reqwest::Error>),
    /// The exam list endpoint returned an HTTP status other than 200 or 401.
    #[error("monitor returned HTTP {0}")]
    HttpStatus(reqwest::StatusCode),
    /// The exam list body exceeded the size limit or did not match the protocol.
    #[error("invalid exam list response")]
    InvalidExamList(#[source] Option<serde_json::Error>),
    /// The monitor WebSocket could not be established or broke the protocol.
    #[error("monitor WebSocket connection failed")]
    WebSocket(#[source] Option<tungstenite::Error>),
    /// The Accoding session is missing, expired, or anonymous.
    #[error("Accoding session is missing or expired")]
    Auth,
    /// The authenticated student is not enrolled in the exam.
    #[error("student is not enrolled in the exam")]
    NotEnrolled,
    /// The exam ended and this candidate has no pending journal to resume.
    #[error("exam has ended and there is no pending local finalization")]
    ExamEnded,
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
