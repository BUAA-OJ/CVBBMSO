//! Client exam runtime for cvbbmso.
//!
//! The crate runs one candidate through one exam as defined in
//! `docs/design/crates/cvbbmso-exam.md`: it keeps the monitor WebSocket alive,
//! executes `start_exam` / `end_exam` commands by driving the recorder, and
//! reports reliable events through a durable local queue that survives
//! disconnects and process restarts.

#![forbid(unsafe_code)]

mod error;
mod journal;
mod monitor;
mod runtime;

use std::path::{Path, PathBuf};

use cvbbmso_accoding::AccodingCookies;
use cvbbmso_protocol::ClientState;
use cvbbmso_recorder::Recorder;
use serde::Serialize;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use url::Url;
use uuid::Uuid;

pub use error::ExamError;
pub use monitor::Monitor;

use crate::runtime::{Launch, Request, Timing};

/// Process-wide settings of the runtime.
#[derive(Debug, Clone)]
pub struct ExamConfig {
    /// HTTPS origin of the monitor server; the WebSocket uses the same origin.
    pub server_base: Url,
    /// Application data root; runtime data lives under `exams/` inside it.
    pub app_data_dir: PathBuf,
    /// Persistent identifier of this device.
    pub device_id: Uuid,
    /// Identifier generated once per client process and kept across reconnects.
    pub instance_id: Uuid,
}

/// Entry point of the exam runtime.
#[derive(Debug)]
pub struct Exam;

/// Notification from the local UI to a running exam.
#[derive(Debug)]
#[non_exhaustive]
pub enum UiNotice {
    /// The browser holds new Accoding cookies to verify for this candidate
    /// before adopting them for reconnects.
    CookiesUpdated(AccodingCookies),
    /// The controlled browser refused a navigation to another origin.
    NavigationDenied {
        /// Origin that was refused.
        target_origin: String,
    },
}

/// Read-only snapshot of the runtime for the trusted local UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientView {
    /// Exam this runtime serves.
    pub exam_id: Uuid,
    /// Whether the monitor connection completed `hello` / `sync`.
    pub connected: bool,
    /// Current execution and recording state.
    pub state: ClientState,
    /// Last protocol error code that needs the student's attention, such as
    /// `ACCODING_AUTH_EXPIRED`; `INTERNAL_ERROR` while local runtime data
    /// cannot be saved.
    pub last_error_code: Option<String>,
}

/// Handle of a running exam.
///
/// Dropping the handle stops the runtime like [`ExamHandle::shutdown`].
#[derive(Debug)]
pub struct ExamHandle {
    requests: mpsc::Sender<Request>,
    view: watch::Receiver<ClientView>,
    task: JoinHandle<()>,
}

impl Exam {
    /// Checks whether this candidate has a local journal awaiting finalization.
    ///
    /// Pass the verified Accoding user id from the exam list. This only reads
    /// the journal for that candidate. A missing, ready, or finished journal
    /// returns `false`.
    ///
    /// # Errors
    /// Returns [`ExamError::Storage`] for an invalid user id or an unreadable
    /// or invalid journal.
    pub async fn has_pending_finalization(
        app_data_dir: &Path,
        exam_id: Uuid,
        oj_user_id: i64,
    ) -> Result<bool, ExamError> {
        let candidate_dir = journal::candidate_dir(app_data_dir, exam_id, oj_user_id)?;
        Ok(journal::Journal::open_pending(&candidate_dir)
            .await?
            .is_some())
    }

    /// Establishes or resumes the runtime of `exam_id` with the given cookies.
    ///
    /// Confirms enrollment and exam phase over HTTP, opens the local data of the
    /// `(exam_id, oj_user_id)` candidate, connects the WebSocket, and returns
    /// after the first `sync`. Internal tasks then keep the connection, the
    /// recording, and the event queue going.
    /// An ended exam can only resume a pending local journal.
    ///
    /// # Errors
    ///
    /// [`ExamError::Http`] when `server_base` is not an HTTPS origin or the monitor is
    /// unreachable, [`ExamError::Auth`] / [`ExamError::NotEnrolled`] when the
    /// session or enrollment is rejected, [`ExamError::ExamEnded`] when there
    /// is no pending journal for an ended exam, [`ExamError::WebSocket`] when
    /// the connection or the first `sync` fails, [`ExamError::Storage`] and
    /// [`ExamError::Recording`] when the local data or the recorder cannot be
    /// opened.
    pub async fn connect(
        config: ExamConfig,
        exam_id: Uuid,
        cookies: AccodingCookies,
    ) -> Result<ExamHandle, ExamError> {
        let monitor = Monitor::new(config.server_base.clone(), None)?;
        runtime::launch(
            Launch {
                config,
                exam_id,
                cookies,
                monitor,
                timing: Timing::default(),
            },
            move |oj_user_id, candidate_dir, policy| {
                Recorder::open(exam_id, oj_user_id, candidate_dir, policy)
            },
        )
        .await
    }
}

impl ExamHandle {
    /// Subscribes to read-only state snapshots for the local UI.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<ClientView> {
        self.view.clone()
    }

    /// Delivers a cookie update or a refused navigation to the runtime.
    ///
    /// # Errors
    ///
    /// [`ExamError::Closed`] when the runtime has stopped.
    pub async fn notify(&self, notice: UiNotice) -> Result<(), ExamError> {
        self.requests
            .send(Request::Notice(notice))
            .await
            .map_err(|_| ExamError::Closed)
    }

    /// Stops capture and seals the current segment, closes the WebSocket, and
    /// keeps the sealed segments; merging and the final summary belong to a
    /// later `Recorder::finish`.
    ///
    /// # Errors
    ///
    /// [`ExamError::Recording`] or [`ExamError::Storage`] when stopping capture
    /// or saving the local data fails, and [`ExamError::Closed`] when the
    /// runtime is gone.
    pub async fn shutdown(self) -> Result<(), ExamError> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(Request::Shutdown(reply))
            .await
            .map_err(|_| ExamError::Closed)?;
        let result = result.await.map_err(|_| ExamError::Closed)?;
        let _ = self.task.await;
        result
    }
}
