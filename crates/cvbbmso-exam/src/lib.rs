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

use std::path::PathBuf;

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
    /// The controlled browser holds new Accoding cookies; used for reconnects.
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
    /// Establishes or resumes the runtime of `exam_id` with the given cookies.
    ///
    /// Confirms enrollment over HTTP, opens the local data of the
    /// `(exam_id, oj_user_id)` candidate, connects the WebSocket, and returns
    /// after the first `sync`. Internal tasks then keep the connection, the
    /// recording, and the event queue going.
    ///
    /// # Errors
    ///
    /// [`ExamError::Http`] when `server_base` is not HTTPS or the monitor is
    /// unreachable, [`ExamError::Auth`] / [`ExamError::NotEnrolled`] when the
    /// session or enrollment is rejected, [`ExamError::WebSocket`] when the
    /// connection or the first `sync` fails, [`ExamError::Storage`] and
    /// [`ExamError::Recording`] when the local data or the recorder cannot be
    /// opened.
    pub async fn connect(
        config: ExamConfig,
        exam_id: Uuid,
        cookies: AccodingCookies,
    ) -> Result<ExamHandle, ExamError> {
        if config.server_base.scheme() != "https" {
            return Err(ExamError::Http(None));
        }
        let monitor = monitor::Monitor::new(config.server_base.clone())?;
        let oj_user_id = monitor.enrollment(&cookies, exam_id).await?;
        runtime::launch(
            Launch {
                config,
                exam_id,
                oj_user_id,
                cookies,
                monitor,
                timing: Timing::default(),
            },
            move |candidate_dir, policy| Recorder::open(exam_id, oj_user_id, candidate_dir, policy),
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
    /// keeps the sealed segments; merging and the final manifest belong to a
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
