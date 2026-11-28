use std::{
    fs::{self, File},
    future::pending,
    io::{self, Read, Write},
    path::Path,
    time::Duration,
};

use cvbbmso_accoding::{AccodingCookies, AccodingError};
use cvbbmso_exam::{ClientView, Exam, ExamConfig, ExamError, ExamHandle, UiNotice};
use cvbbmso_protocol::{ExecutionState, error_code};
use serde::Serialize;
use tauri::{AppHandle, Emitter, EventTarget, Manager, RunEvent, State, async_runtime};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::sleep,
};
use url::{Origin, Url};
use uuid::Uuid;

use super::{
    Desktop, OJ, ReadSessionError, UI,
    admission::{Admission, CleanupError, Failure, Status},
    current_cookies,
};
use crate::{monitor::MonitorError, policy::denied_target_origin, preflight::Preflight};

const STATUS_EVENT: &str = "cvbbmso-status";
const COOKIE_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

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
    ExamEnded,
    StorageFailed,
    RecordingFailed,
    ShutdownFailed,
    ReportingPending,
}

impl From<ExamError> for ExamCommandError {
    fn from(error: ExamError) -> Self {
        match error {
            ExamError::Auth => Self::SignInRequired,
            ExamError::NotEnrolled => Self::NotEnrolled,
            ExamError::ExamEnded => Self::ExamEnded,
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
    if !app.state::<Admission>().is_ready() {
        return Err(ExamCommandError::AdmissionUnavailable);
    }
    Ok(())
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
    AwaitingConfirmation(i32),
    Drained(i32),
}

pub(super) struct ExamSession {
    requests: mpsc::Sender<Request>,
    snapshot: watch::Receiver<Option<ClientView>>,
    exit: watch::Sender<Exit>,
    notices: watch::Sender<Option<mpsc::Sender<Notice>>>,
}

impl ExamSession {
    pub(super) fn can_close(&self) -> bool {
        !reporting_pending(self.snapshot.borrow().as_ref())
    }

    pub(super) fn launch(app: &AppHandle) {
        let (requests, incoming) = mpsc::channel(8);
        let (publish, snapshot) = watch::channel(None);
        let (exit, exiting) = watch::channel(Exit::Running);
        let (notices, _) = watch::channel(None);
        app.manage(Self {
            requests,
            snapshot,
            exit,
            notices,
        });
        async_runtime::spawn(run(app.clone(), incoming, publish, exiting, Uuid::new_v4()));
    }

    pub(super) fn needs_sign_in(&self) -> bool {
        self.snapshot.borrow().as_ref().is_some_and(|view| {
            view.state.execution != ExecutionState::Finished
                && view.last_error_code.as_deref() == Some(error_code::ACCODING_AUTH_EXPIRED)
        })
    }

    pub(super) fn cookie_retry(&self) -> Option<CookieRetry> {
        self.notices.borrow().clone().map(CookieRetry)
    }

    pub(super) fn navigation_denied(&self, allowed: &Origin, url: &Url) {
        let current = self.notices.borrow().clone();
        if let Some(current) = current
            && let Some(target_origin) = denied_target_origin(allowed, url)
        {
            // WebView callbacks cannot wait for the runtime's queue to drain.
            let _ = current.try_send(Notice::Navigation(target_origin));
        }
    }
}

pub(super) fn handle_exit(app: &AppHandle, event: &RunEvent) {
    if let RunEvent::ExitRequested { api, code, .. } = event
        && let Some(sessions) = app.try_state::<ExamSession>()
    {
        let exit = *sessions.exit.borrow();
        match exit {
            Exit::Running => {
                api.prevent_exit();
                #[cfg(target_os = "macos")]
                if !app.state::<Admission>().request_exit(app) {
                    return;
                }
                sessions
                    .exit
                    .send_replace(Exit::Requested(code.unwrap_or(0)));
            }
            Exit::Requested(_) => api.prevent_exit(),
            Exit::AwaitingConfirmation(code) => {
                api.prevent_exit();
                sessions.exit.send_replace(Exit::Drained(code));
                app.exit(code);
            }
            Exit::Drained(_) => {}
        }
    }
}

async fn finish_exit(app: &AppHandle, code: i32, cleanup: Result<(), CleanupError>) {
    if let Err(error) = cleanup {
        let report = error.report(&app.state::<Desktop>().data_dir).await;
        let current = app.clone();
        if app
            .run_on_main_thread(move || {
                let sessions = current.state::<ExamSession>();
                if let Some(window) = current.get_webview_window(UI)
                    && window.show().is_ok()
                {
                    let _ = window.unminimize();
                    if let Some(browser) = current.get_webview_window(OJ) {
                        let _ = browser.destroy();
                    }
                    sessions.exit.send_replace(Exit::AwaitingConfirmation(code));
                    current
                        .state::<Admission>()
                        .show_cleanup_failure(&current, report);
                    let _ = window.set_focus();
                } else {
                    sessions.exit.send_replace(Exit::Drained(code));
                    current.exit(code);
                }
            })
            .is_ok()
        {
            return;
        }
    }
    app.state::<ExamSession>()
        .exit
        .send_replace(Exit::Drained(code));
    app.exit(code);
}

pub(super) fn exit_code(app: &AppHandle, runtime_code: i32) -> i32 {
    // Tauri delivers the requested code but the runtime may return zero.
    if let Some(sessions) = app.try_state::<ExamSession>()
        && let Exit::Drained(code) = *sessions.exit.borrow()
    {
        return if code == 0 { runtime_code } else { code };
    }
    runtime_code
}

#[cfg(test)]
pub(super) fn exit_drained(app: &AppHandle) -> bool {
    matches!(*app.state::<ExamSession>().exit.borrow(), Exit::Drained(_))
}

type ConnectedExam = (ExamHandle, AccodingCookies);

struct Opening {
    task: JoinHandle<Result<ConnectedExam, ExamCommandError>>,
    reply: Reply,
}

struct Active {
    handle: ExamHandle,
    updates: watch::Receiver<ClientView>,
    inbox: Option<NoticeInbox>,
}

impl Active {
    fn reporting_pending(&self) -> bool {
        reporting_pending(Some(&self.updates.borrow()))
    }
}

#[derive(Clone)]
enum Notice {
    Cookies(AccodingCookies),
    Navigation(String),
}

pub(super) struct CookieRetry(mpsc::Sender<Notice>);

impl CookieRetry {
    pub(super) async fn submit(self, cookies: AccodingCookies) {
        // The sender belongs to the session that was active before the read.
        let _ = self.0.send(Notice::Cookies(cookies)).await;
    }
}

impl From<Notice> for UiNotice {
    fn from(notice: Notice) -> Self {
        match notice {
            Notice::Cookies(cookies) => Self::CookiesUpdated(cookies),
            Notice::Navigation(target_origin) => Self::NavigationDenied { target_origin },
        }
    }
}

struct NoticeInbox {
    incoming: mpsc::Receiver<Notice>,
    pending: Option<Notice>,
}

impl NoticeInbox {
    fn start(app: &AppHandle, initial: AccodingCookies) -> Self {
        let (outgoing, incoming) = mpsc::channel(16);
        app.state::<ExamSession>()
            .notices
            .send_replace(Some(outgoing.clone()));
        tokio::spawn(sample_cookies(app.clone(), initial, outgoing));
        Self {
            incoming,
            pending: None,
        }
    }

    async fn deliver(&mut self, handle: &ExamHandle) {
        if self.pending.is_none() {
            self.pending = self.incoming.recv().await;
        }
        let Some(notice) = self.pending.clone() else {
            return pending().await;
        };
        // Keep the notice in the inbox if the actor's select cancels this send.
        let _ = handle.notify(notice.into()).await;
        self.pending = None;
    }
}

async fn forward_notice(delivery: Option<(&ExamHandle, &mut NoticeInbox)>) {
    match delivery {
        Some((handle, inbox)) => inbox.deliver(handle).await,
        None => pending().await,
    }
}

async fn sample_cookies(app: AppHandle, mut last: AccodingCookies, outgoing: mpsc::Sender<Notice>) {
    loop {
        let observed = tokio::select! {
            biased;
            () = outgoing.closed() => return,
            result = async {
                sleep(COOKIE_SAMPLE_INTERVAL).await;
                current_cookies(&app).await
            } => result,
        };
        if let Ok(cookies) = observed
            && !same_cookies(&last, &cookies)
        {
            last = cookies.clone();
            if outgoing.send(Notice::Cookies(cookies)).await.is_err() {
                return;
            }
        }
    }
}

fn same_cookies(previous: &AccodingCookies, observed: &AccodingCookies) -> bool {
    fn ordered_pairs(header: &[u8]) -> Vec<&[u8]> {
        let mut pairs: Vec<_> = header
            .split(|byte| *byte == b';')
            .map(<[u8]>::trim_ascii)
            .collect();
        // Preserve the precedence of duplicate names from different cookie paths.
        pairs.sort_by_key(|pair| pair.split(|byte| *byte == b'=').next());
        pairs
    }
    let previous = previous.header_value();
    let observed = observed.header_value();
    previous == observed || ordered_pairs(previous.as_bytes()) == ordered_pairs(observed.as_bytes())
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
    let sessions = app.state::<ExamSession>();
    let admission = app.state::<Admission>();
    let mut admission_updates = admission.subscribe();
    let mut guard_close_failed = false;
    loop {
        let (view_updates, delivery) = active.as_mut().map_or((None, None), |active| {
            (
                Some(&mut active.updates),
                active.inbox.as_mut().map(|inbox| (&active.handle, inbox)),
            )
        });
        tokio::select! {
            biased;
            _ = exit.changed() => {
                let requested = *exit.borrow();
                if let Exit::Requested(code) = requested {
                    if code == 0 && (opening.is_some() || active.as_ref().is_some_and(Active::reporting_pending)) {
                        sessions.exit.send_replace(Exit::Running);
                        let _ = app.emit_to(EventTarget::webview(UI), "confirm-exit", ());
                        continue;
                    }
                    admission.begin_exit(&app);
                    let result = clear(&sessions, &mut opening, &mut active, &snapshot).await;
                    let cleanup = admission.shutdown().await;
                    let failed = result.is_err() || cleanup.is_err() || guard_close_failed;
                    let exit_code = if failed && code == 0 { 1 } else { code };
                    finish_exit(&app, exit_code, cleanup).await;
                    return;
                }
            }
            _ = admission_updates.changed() => {
                let status = *admission_updates.borrow_and_update();
                if status == Status::Blocked(Failure::GuardLost) {
                    guard_close_failed |= clear(&sessions, &mut opening, &mut active, &snapshot)
                        .await.is_err();
                }
            }
            request = requests.recv() => match request {
                Some(Request::Open(_, reply)) if !admission.is_ready() => {
                    let _ = reply.send(Err(ExamCommandError::AdmissionUnavailable));
                }
                Some(Request::Open(_, reply)) if opening.is_some() || active.is_some() => {
                    let _ = reply.send(Err(ExamCommandError::SessionBusy));
                }
                Some(Request::Open(exam_id, reply)) => {
                    opening = Some(Opening {
                        task: tokio::spawn(connect(app.clone(), exam_id, instance_id)),
                        reply,
                    });
                }
                Some(Request::Close(reply)) if opening.is_some() || active.as_ref().is_some_and(Active::reporting_pending) => {
                    let _ = reply.send(Err(ExamCommandError::ReportingPending));
                }
                Some(Request::Close(reply)) => {
                    let _ = reply.send(clear(&sessions, &mut opening, &mut active, &snapshot).await);
                }
                None => {
                    let _ = clear(&sessions, &mut opening, &mut active, &snapshot).await;
                    return;
                }
            },
            result = joining(&mut opening) => {
                if let Some(Opening { reply, .. }) = opening.take() {
                    if !admission.is_ready() {
                        if let Ok((handle, _)) = result {
                            guard_close_failed |= handle.shutdown().await.is_err();
                        }
                        let _ = reply.send(Err(ExamCommandError::AdmissionUnavailable));
                        continue;
                    }
                    let result = result.map(|(handle, cookies)| {
                        let mut updates = handle.subscribe();
                        let view = updates.borrow_and_update().clone();
                        let inbox = (view.state.execution != ExecutionState::Finished)
                            .then(|| NoticeInbox::start(&app, cookies));
                        active = Some(Active { handle, updates, inbox });
                        publish(&app, &snapshot, view);
                    });
                    let _ = reply.send(result);
                }
            }
            view = next_view(view_updates) => {
                if view.state.execution == ExecutionState::Finished
                    && let Some(active) = &mut active
                {
                    sessions.notices.send_replace(None);
                    active.inbox = None;
                }
                publish(&app, &snapshot, view);
            }
            () = forward_notice(delivery) => {}
        }
    }
}

fn reporting_pending(view: Option<&ClientView>) -> bool {
    view.is_some_and(|view| view.state.execution != ExecutionState::Finished)
}

async fn joining(opening: &mut Option<Opening>) -> Result<ConnectedExam, ExamCommandError> {
    match opening {
        Some(opening) => (&mut opening.task)
            .await
            .map_err(|_| ExamCommandError::SessionClosed)?,
        None => pending().await,
    }
}

async fn next_view(updates: Option<&mut watch::Receiver<ClientView>>) -> ClientView {
    if let Some(updates) = updates
        && updates.changed().await.is_ok()
    {
        return updates.borrow_and_update().clone();
    }
    pending().await
}

async fn clear(
    sessions: &ExamSession,
    opening: &mut Option<Opening>,
    active: &mut Option<Active>,
    snapshot: &watch::Sender<Option<ClientView>>,
) -> Result<(), ExamCommandError> {
    sessions.notices.send_replace(None);
    snapshot.send_replace(None);
    let handle = if let Some(opening) = opening.take() {
        let _ = opening.reply.send(Err(ExamCommandError::SessionClosed));
        // Finish an in-flight open before releasing the slot, so a new open
        // cannot overlap journal or recorder recovery for the same candidate.
        opening
            .task
            .await
            .ok()
            .and_then(Result::ok)
            .map(|(handle, _)| handle)
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
) -> Result<ConnectedExam, ExamCommandError> {
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
    Exam::connect(config, exam_id, cookies.clone())
        .await
        .map(|handle| (handle, cookies))
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
