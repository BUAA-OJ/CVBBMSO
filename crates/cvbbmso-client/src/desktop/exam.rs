use std::{
    fs::{self, File},
    future::pending,
    io::{self, Read, Write},
    path::Path,
};

use cvbbmso_accoding::AccodingError;
use cvbbmso_exam::{ClientView, Exam, ExamConfig, ExamError, ExamHandle};
use cvbbmso_protocol::error_code;
use serde::Serialize;
use tauri::{AppHandle, Emitter, EventTarget, Manager, RunEvent, State, async_runtime};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use uuid::Uuid;

use super::{Desktop, ReadSessionError, UI, current_cookies};
use crate::{monitor::MonitorError, preflight::Preflight};

const STATUS_EVENT: &str = "cvbbmso-status";

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ExamCommandError {
    PreflightBlocked,
    AdmissionUnavailable,
    SessionBusy,
    SessionClosed,
    SignInRequired,
    SessionUnreadable,
    MonitorUnconfigured,
    ConnectionFailed,
    NotEnrolled,
    StorageFailed,
    RecordingFailed,
    ShutdownFailed,
}

impl From<ExamError> for ExamCommandError {
    fn from(error: ExamError) -> Self {
        match error {
            ExamError::Auth => Self::SignInRequired,
            ExamError::NotEnrolled => Self::NotEnrolled,
            ExamError::Storage(_) => Self::StorageFailed,
            ExamError::Recording(_) => Self::RecordingFailed,
            ExamError::Closed => Self::SessionClosed,
            _ => Self::ConnectionFailed,
        }
    }
}

#[derive(Serialize)]
pub(super) struct Connected {
    connected: bool,
}

#[derive(Serialize)]
pub(super) struct Closed {
    closed: bool,
}

#[tauri::command(rename_all = "snake_case")]
pub(super) async fn open_exam(
    app: AppHandle,
    exam_id: Uuid,
) -> Result<Connected, ExamCommandError> {
    check_admission(&app)?;
    let sessions = app.state::<ExamSession>();
    let (reply, result) = oneshot::channel();
    sessions
        .requests
        .try_send(Request::Open(exam_id, reply))
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => ExamCommandError::SessionBusy,
            mpsc::error::TrySendError::Closed(_) => ExamCommandError::SessionClosed,
        })?;
    result
        .await
        .map_err(|_| ExamCommandError::SessionClosed)??;
    Ok(Connected { connected: true })
}

fn check_admission(app: &AppHandle) -> Result<(), ExamCommandError> {
    if !app.state::<Preflight>().is_admitted() {
        return Err(ExamCommandError::PreflightBlocked);
    }
    #[cfg(test)]
    if app.state::<Desktop>().fixture_exam_admission {
        return Ok(());
    }
    // The network guard and remaining admission checks are not implemented.
    Err(ExamCommandError::AdmissionUnavailable)
}

#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri injects managed state by value"
)]
pub(super) fn get_exam_status(sessions: State<'_, ExamSession>) -> Option<ClientView> {
    sessions.snapshot.borrow().clone()
}

#[tauri::command]
pub(super) async fn close_exam(
    sessions: State<'_, ExamSession>,
) -> Result<Closed, ExamCommandError> {
    let (reply, result) = oneshot::channel();
    sessions
        .requests
        .send(Request::Close(reply))
        .await
        .map_err(|_| ExamCommandError::SessionClosed)?;
    result
        .await
        .map_err(|_| ExamCommandError::SessionClosed)??;
    Ok(Closed { closed: true })
}

type Reply = oneshot::Sender<Result<(), ExamCommandError>>;

enum Request {
    Open(Uuid, Reply),
    Close(Reply),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Exit {
    Running,
    Requested(i32),
    Drained,
}

pub(super) struct ExamSession {
    requests: mpsc::Sender<Request>,
    snapshot: watch::Receiver<Option<ClientView>>,
    exit: watch::Sender<Exit>,
}

impl ExamSession {
    pub(super) fn launch(app: &AppHandle) {
        let (requests, incoming) = mpsc::channel(8);
        let (publish, snapshot) = watch::channel(None);
        let (exit, exiting) = watch::channel(Exit::Running);
        app.manage(Self {
            requests,
            snapshot,
            exit,
        });
        async_runtime::spawn(run(app.clone(), incoming, publish, exiting, Uuid::new_v4()));
    }
}

pub(super) fn handle_exit(app: &AppHandle, event: &RunEvent) {
    if let RunEvent::ExitRequested { api, code, .. } = event
        && let Some(sessions) = app.try_state::<ExamSession>()
    {
        if *sessions.exit.borrow() == Exit::Drained {
            return;
        }
        api.prevent_exit();
        sessions.exit.send_if_modified(|state| {
            if *state == Exit::Running {
                *state = Exit::Requested(code.unwrap_or(0));
                return true;
            }
            false
        });
    }
}

#[cfg(test)]
pub(super) fn exit_drained(app: &AppHandle) -> bool {
    *app.state::<ExamSession>().exit.borrow() == Exit::Drained
}

struct Opening {
    task: JoinHandle<Result<ExamHandle, ExamCommandError>>,
    reply: Reply,
}

struct Active {
    handle: ExamHandle,
    updates: watch::Receiver<ClientView>,
}

async fn run(
    app: AppHandle,
    mut requests: mpsc::Receiver<Request>,
    snapshot: watch::Sender<Option<ClientView>>,
    mut exit: watch::Receiver<Exit>,
    instance_id: Uuid,
) {
    let mut opening: Option<Opening> = None;
    let mut active: Option<Active> = None;
    loop {
        tokio::select! {
            biased;
            _ = exit.changed() => {
                let requested = *exit.borrow();
                if let Exit::Requested(code) = requested {
                    let result = clear(&mut opening, &mut active, &snapshot).await;
                    app.state::<ExamSession>().exit.send_replace(Exit::Drained);
                    app.exit(if result.is_err() && code == 0 { 1 } else { code });
                }
                return;
            }
            request = requests.recv() => match request {
                Some(Request::Open(_, reply)) if opening.is_some() || active.is_some() => {
                    let _ = reply.send(Err(ExamCommandError::SessionBusy));
                }
                Some(Request::Open(exam_id, reply)) => {
                    opening = Some(Opening {
                        task: tokio::spawn(connect(app.clone(), exam_id, instance_id)),
                        reply,
                    });
                }
                Some(Request::Close(reply)) => {
                    let _ = reply.send(clear(&mut opening, &mut active, &snapshot).await);
                }
                None => {
                    let _ = clear(&mut opening, &mut active, &snapshot).await;
                    return;
                }
            },
            result = joining(&mut opening) => {
                if let Some(Opening { reply, .. }) = opening.take() {
                    let result = result.map(|handle| {
                        let mut updates = handle.subscribe();
                        let view = updates.borrow_and_update().clone();
                        active = Some(Active { handle, updates });
                        publish(&app, &snapshot, view);
                    });
                    let _ = reply.send(result);
                }
            }
            view = next_view(&mut active) => publish(&app, &snapshot, view),
        }
    }
}

async fn joining(opening: &mut Option<Opening>) -> Result<ExamHandle, ExamCommandError> {
    match opening {
        Some(opening) => (&mut opening.task)
            .await
            .map_err(|_| ExamCommandError::SessionClosed)?,
        None => pending().await,
    }
}

async fn next_view(active: &mut Option<Active>) -> ClientView {
    if let Some(active) = active
        && active.updates.changed().await.is_ok()
    {
        return active.updates.borrow_and_update().clone();
    }
    pending().await
}

async fn clear(
    opening: &mut Option<Opening>,
    active: &mut Option<Active>,
    snapshot: &watch::Sender<Option<ClientView>>,
) -> Result<(), ExamCommandError> {
    snapshot.send_replace(None);
    let handle = if let Some(opening) = opening.take() {
        let _ = opening.reply.send(Err(ExamCommandError::SessionClosed));
        // Finish an in-flight open before releasing the slot, so a new open
        // cannot overlap journal or recorder recovery for the same candidate.
        opening.task.await.ok().and_then(Result::ok)
    } else {
        active.take().map(|active| active.handle)
    };
    if let Some(handle) = handle {
        handle
            .shutdown()
            .await
            .map_err(|_| ExamCommandError::ShutdownFailed)?;
    }
    Ok(())
}

fn publish(app: &AppHandle, snapshot: &watch::Sender<Option<ClientView>>, mut view: ClientView) {
    // The server's error frame carries free text. Expose only protocol codes
    // so a server diagnostic cannot reflect credentials into the WebView.
    if let Some(code) = view.last_error_code.as_mut()
        && !matches!(
            code.as_str(),
            error_code::INVALID_REQUEST
                | error_code::ACCODING_AUTH_EXPIRED
                | error_code::NOT_ENROLLED
                | error_code::EXAM_NOT_FOUND
                | error_code::ACCODING_UNAVAILABLE
                | error_code::INTERNAL_ERROR
                | error_code::PROTOCOL_ERROR
                | error_code::COMMAND_STATE_CONFLICT
                | error_code::RECORDING_START_FAILED
                | error_code::RECORDING_FINISH_FAILED
        )
    {
        error_code::INTERNAL_ERROR.clone_into(code);
    }
    snapshot.send_replace(Some(view.clone()));
    let _ = app.emit_to(EventTarget::webview(UI), STATUS_EVENT, view);
}

async fn connect(
    app: AppHandle,
    exam_id: Uuid,
    instance_id: Uuid,
) -> Result<ExamHandle, ExamCommandError> {
    let desktop = app.state::<Desktop>();
    let monitor = desktop.monitor.as_ref().map_err(|error| match error {
        MonitorError::Configuration => ExamCommandError::MonitorUnconfigured,
        _ => ExamCommandError::ConnectionFailed,
    })?;
    let cookies = current_cookies(&app).await.map_err(|error| match error {
        ReadSessionError::BrowserClosed
        | ReadSessionError::Invalid(AccodingError::MissingSession) => {
            ExamCommandError::SignInRequired
        }
        ReadSessionError::Native | ReadSessionError::Invalid(_) => {
            ExamCommandError::SessionUnreadable
        }
    })?;
    let device_dir = desktop.data_dir.clone();
    let device_id = async_runtime::spawn_blocking(move || load_device_id(&device_dir))
        .await
        .map_err(|_| ExamCommandError::StorageFailed)?
        .map_err(|_| ExamCommandError::StorageFailed)?;
    let config = ExamConfig {
        server_base: monitor.server_base().clone(),
        app_data_dir: desktop.data_dir.clone(),
        device_id,
        instance_id,
    };
    Exam::connect(config, exam_id, cookies)
        .await
        .map_err(ExamCommandError::from)
}

fn load_device_id(data_dir: &Path) -> io::Result<Uuid> {
    let path = data_dir.join("device-id");
    let read = || {
        let mut bytes = Vec::new();
        File::open(&path)?.take(37).read_to_end(&mut bytes)?;
        Uuid::try_parse_ascii(&bytes)
            .ok()
            .filter(|id| !id.is_nil() && bytes == id.hyphenated().to_string().as_bytes())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid device identifier"))
    };
    match read() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        existing => return existing,
    }
    fs::create_dir_all(data_dir)?;
    let id = Uuid::new_v4();
    let mut file = tempfile::NamedTempFile::new_in(data_dir)?;
    write!(file, "{id}")?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(&path) {
        Ok(_) => Ok(id),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => read(),
        Err(error) => Err(error.error),
    }
}
