//! The runtime task of one candidate in one exam.
//!
//! A single task owns the journal, the recorder, and the client state, so
//! every decision is made in one place and in arrival order. The WebSocket
//! lives in a separate session task (see [`session`]) and is replaced on every
//! reconnect.
//!
//! Server time is anchored at each `sync` and advanced with the monotonic
//! clock, so command deadlines and the offline end deadline do not depend on
//! the local wall clock, which the student controls.

mod capture;
mod session;

use std::{
    collections::HashMap,
    future::Future,
    hash::{BuildHasher, Hasher, RandomState},
    io,
    path::PathBuf,
    time::Duration,
};

use cvbbmso_accoding::AccodingCookies;
use cvbbmso_protocol::{
    ClientHello, ClientIncident, ClientMessage, ClientState, CommandName, CommandStatus, ExamEvent,
    ExamPhase, ExecutionState, IncidentCode, Platform, RecordingPolicy, RecordingState,
    RecordingSummary, ServerMessage, error_code,
};
use cvbbmso_recorder::{RecorderEvent, RecordingError};
use time::OffsetDateTime;
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::{Instant, sleep, sleep_until, timeout, timeout_at},
};
use uuid::Uuid;

pub(crate) use capture::Capture;
use session::{Inbound, Liveness};

use crate::{
    ClientView, ExamConfig, ExamError, ExamHandle, UiNotice,
    journal::{Journal, candidate_dir},
    monitor::Monitor,
};

/// Grace period after `end_at` before an offline client stops recording on
/// its own, fixed by the design contract.
const END_GRACE: time::Duration = time::Duration::seconds(60);
/// Longest detail kept for a navigation incident.
const MAX_DETAIL_CHARS: usize = 200;
/// Repeated denials of one origin within this window are reported once.
const NAVIGATION_QUIET: Duration = Duration::from_secs(5);

/// Durations and retry bounds of the runtime.
///
/// Production uses [`Timing::default`]; tests shorten the waits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// Heartbeat cadence and dead-connection detection; the silence also
    /// bounds the handshake and the wait for the first `sync`.
    pub(crate) liveness: Liveness,
    /// First reconnect delay; doubled after each failure.
    pub(crate) reconnect_min: Duration,
    /// Longest reconnect delay.
    pub(crate) reconnect_max: Duration,
    /// Delay between recorder attempts.
    pub(crate) retry: Duration,
    /// Delay between capture attempts once `start_attempts` were used up.
    pub(crate) retry_max: Duration,
    /// Attempts to start capture before `start_exam` reports failure; capture
    /// is still retried afterwards, every `retry_max`.
    pub(crate) start_attempts: u32,
    /// Attempts to finish the recording before `end_exam` fails.
    pub(crate) finish_attempts: u32,
    /// Wait for `command_ack` before the connection is replaced.
    pub(crate) ack_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            liveness: Liveness {
                heartbeat: Duration::from_secs(5),
                silence: Duration::from_secs(20),
            },
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(30),
            retry: Duration::from_secs(3),
            retry_max: Duration::from_secs(60),
            start_attempts: 5,
            finish_attempts: 3,
            ack_timeout: Duration::from_secs(30),
        }
    }
}

/// Request from an [`ExamHandle`].
#[derive(Debug)]
pub(crate) enum Request {
    Notice(UiNotice),
    Shutdown(oneshot::Sender<Result<(), ExamError>>),
}

/// Everything known before the runtime starts.
pub(crate) struct Launch {
    pub(crate) config: ExamConfig,
    pub(crate) exam_id: Uuid,
    pub(crate) cookies: AccodingCookies,
    pub(crate) monitor: Monitor,
    pub(crate) timing: Timing,
}

/// Verifies enrollment, opens the journal, connects, waits for the first
/// `sync`, opens the recorder, and starts the runtime task.
pub(crate) async fn launch<C, F, Fut>(launch: Launch, open: F) -> Result<ExamHandle, ExamError>
where
    C: Capture,
    F: FnOnce(i64, PathBuf, RecordingPolicy) -> Fut,
    Fut: Future<Output = Result<(C, mpsc::Receiver<RecorderEvent>), RecordingError>>,
{
    let Launch {
        config,
        exam_id,
        cookies,
        monitor,
        timing,
    } = launch;
    let (oj_user_id, phase) = monitor.enrollment(&cookies, exam_id).await?;
    let candidate_dir = candidate_dir(&config.app_data_dir, exam_id, oj_user_id)?;
    let journal = if phase == ExamPhase::Ended {
        Journal::open_pending(&candidate_dir)
            .await?
            .ok_or(ExamError::ExamEnded)?
    } else {
        Journal::open(&candidate_dir).await?
    };
    let state = resumed(journal.state());

    let (inbound_tx, mut inbound) = mpsc::channel(64);
    let session = session::spawn(
        monitor.socket_request(&cookies, exam_id)?,
        1,
        inbound_tx.clone(),
        timing.liveness,
    );
    let hello = hello(config.device_id, config.instance_id, state);
    let (link, sync, received_at) =
        timeout(timing.liveness.silence, first_sync(&mut inbound, hello))
            .await
            .map_err(|_| ExamError::WebSocket(None))??;
    let ServerMessage::Sync {
        phase,
        recording_policy,
        ..
    } = &sync
    else {
        return Err(ExamError::WebSocket(None));
    };
    // The exam may have ended since the HTTP enrollment check.
    if *phase == ExamPhase::Ended && !journal.has_pending_finalization() {
        return Err(ExamError::ExamEnded);
    }
    let (capture, recorder) = open(oj_user_id, candidate_dir, *recording_policy)
        .await
        .map_err(ExamError::Recording)?;

    let (requests_tx, requests) = mpsc::channel(16);
    let (view_tx, view) = watch::channel(ClientView {
        exam_id,
        connected: false,
        state,
        last_error_code: None,
    });
    let mut runtime = Runtime {
        exam_id,
        oj_user_id,
        device_id: config.device_id,
        instance_id: config.instance_id,
        monitor,
        cookies,
        timing,
        journal,
        capture,
        recorder,
        inbound_tx,
        inbound,
        requests,
        view: view_tx,
        session: Some(session),
        link: Some(link),
        generation: 1,
        connected: false,
        state,
        last_error_code: None,
        clock: None,
        end_at: None,
        command: None,
        reconnect_at: None,
        backoff: timing.reconnect_min,
        retry_at: None,
        start_failures: 0,
        ack_deadline: None,
        recent_denials: HashMap::new(),
        done: false,
        storage_retry_at: None,
        sent_through: 0,
    };
    runtime.handle_message(sync, received_at).await;
    if state.execution == ExecutionState::Finished {
        // Nothing is left to report; keep no connection to an ended exam.
        runtime.done = true;
        runtime.drop_link();
    }
    runtime.publish();
    let task = tokio::spawn(runtime.run());
    Ok(ExamHandle {
        requests: requests_tx,
        view,
        task,
    })
}

/// Waits for the session to open, sends `hello`, and returns the first `sync`.
async fn first_sync(
    inbound: &mut mpsc::Receiver<Inbound>,
    hello: ClientMessage,
) -> Result<(mpsc::Sender<ClientMessage>, ServerMessage, Instant), ExamError> {
    let link = match inbound.recv().await {
        Some(Inbound::Opened(_, link)) => link,
        Some(Inbound::Closed(_, error)) => return Err(error.unwrap_or(ExamError::WebSocket(None))),
        _ => return Err(ExamError::WebSocket(None)),
    };
    link.try_send(hello)
        .map_err(|_| ExamError::WebSocket(None))?;
    match inbound.recv().await {
        Some(Inbound::Message {
            message: sync @ ServerMessage::Sync { .. },
            received_at,
            ..
        }) => Ok((link, sync, received_at)),
        Some(Inbound::Message {
            message: ServerMessage::Error { code, .. },
            ..
        }) => Err(code_error(&code)),
        Some(Inbound::Closed(_, Some(error))) => Err(error),
        _ => Err(ExamError::WebSocket(None)),
    }
}

/// State to report after the process (re)opens a journal.
///
/// Capture never survives a process, so a candidate that was recording
/// reports a capture failure: the server then neither repeats `start_exam`
/// nor misses `end_exam`, and the runtime resumes capture itself while the
/// exam runs.
fn resumed(state: ClientState) -> ClientState {
    if state.execution == ExecutionState::Recording {
        ClientState {
            execution: ExecutionState::Recording,
            recording: RecordingState::Failed,
        }
    } else {
        state
    }
}

fn hello(device_id: Uuid, instance_id: Uuid, state: ClientState) -> ClientMessage {
    ClientMessage::Hello(ClientHello {
        device_id,
        instance_id,
        platform: platform(),
        state,
    })
}

fn platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Macos
    } else {
        Platform::Windows
    }
}

fn code_error(code: &str) -> ExamError {
    match code {
        error_code::ACCODING_AUTH_EXPIRED => ExamError::Auth,
        error_code::NOT_ENROLLED | error_code::EXAM_NOT_FOUND => ExamError::NotEnrolled,
        _ => ExamError::WebSocket(None),
    }
}

/// Server time anchored at the last `sync`.
#[derive(Debug, Clone, Copy)]
struct Clock {
    server_time: OffsetDateTime,
    at: Instant,
}

impl Clock {
    fn now(&self) -> OffsetDateTime {
        let elapsed = time::Duration::try_from(self.at.elapsed()).unwrap_or(time::Duration::MAX);
        self.server_time.saturating_add(elapsed)
    }

    /// Local instant of a server time; instants in the past map to the anchor.
    fn instant_of(&self, time: OffsetDateTime) -> Instant {
        self.at + Duration::try_from(time - self.server_time).unwrap_or_default()
    }
}

/// Command received from the server and not yet acknowledged.
#[derive(Debug)]
struct Active {
    id: Uuid,
    name: CommandName,
    due: Instant,
    started: bool,
    result: Option<(CommandStatus, Option<String>)>,
}

struct Runtime<C> {
    exam_id: Uuid,
    oj_user_id: i64,
    device_id: Uuid,
    instance_id: Uuid,
    monitor: Monitor,
    cookies: AccodingCookies,
    timing: Timing,
    journal: Journal,
    capture: C,
    recorder: mpsc::Receiver<RecorderEvent>,
    inbound_tx: mpsc::Sender<Inbound>,
    inbound: mpsc::Receiver<Inbound>,
    requests: mpsc::Receiver<Request>,
    view: watch::Sender<ClientView>,
    /// Dropped to cancel the session, including an in-flight handshake or send.
    session: Option<oneshot::Sender<()>>,
    /// Outbound queue of the current session, once its handshake succeeded.
    link: Option<mpsc::Sender<ClientMessage>>,
    generation: u64,
    /// Whether the current session completed `hello`/`sync`.
    connected: bool,
    state: ClientState,
    last_error_code: Option<String>,
    clock: Option<Clock>,
    end_at: Option<OffsetDateTime>,
    command: Option<Active>,
    reconnect_at: Option<Instant>,
    backoff: Duration,
    retry_at: Option<Instant>,
    start_failures: u32,
    ack_deadline: Option<Instant>,
    recent_denials: HashMap<String, Instant>,
    /// The end handshake completed; the runtime no longer reconnects.
    done: bool,
    /// Retries a failed journal save even if no further events arrive.
    storage_retry_at: Option<Instant>,
    /// Highest event sequence queued to the current session.
    sent_through: i64,
}

impl<C: Capture> Runtime<C> {
    async fn run(mut self) {
        let mut cookie_check = None;
        loop {
            self.publish();
            let deadline = self.next_deadline();
            tokio::select! {
                request = self.requests.recv() => match request {
                    Some(Request::Notice(UiNotice::CookiesUpdated(cookies))) => {
                        if !self.done {
                            let monitor = self.monitor.clone();
                            let (exam_id, oj_user_id) = (self.exam_id, self.oj_user_id);
                            // Replacing the future cancels a superseded browser update.
                            cookie_check = Some(Box::pin(async move {
                                let (updated_user_id, _) =
                                    monitor.enrollment(&cookies, exam_id).await?;
                                if updated_user_id != oj_user_id {
                                    return Err(ExamError::Auth);
                                }
                                Ok(cookies)
                            }));
                        }
                    }
                    Some(Request::Notice(UiNotice::NavigationDenied { target_origin })) => {
                        self.navigation_denied(target_origin).await;
                    }
                    Some(Request::Shutdown(reply)) => {
                        let _ = reply.send(self.stop().await);
                        return;
                    }
                    None => {
                        let _ = self.stop().await;
                        return;
                    }
                },
                result = async {
                    match cookie_check.as_mut() {
                        Some(check) => check.await,
                        None => std::future::pending().await,
                    }
                } => {
                    cookie_check = None;
                    if !self.done {
                        self.apply_cookie_check(result);
                    }
                },
                Some(inbound) = self.inbound.recv() => self.handle_inbound(inbound).await,
                Some(event) = self.recorder.recv() => self.recorder_event(event).await,
                () = sleep_until_some(deadline) => self.tick().await,
            }
        }
    }

    async fn stop(&mut self) -> Result<(), ExamError> {
        self.drop_link();
        self.capture.stop().await.map_err(ExamError::Recording)?;
        self.journal.save().await
    }

    /// Writes the journal, keeping the runtime alive when the write fails.
    ///
    /// A successful save releases all new events in sequence order. On
    /// failure, keep them in memory and retry the whole snapshot.
    async fn persist(&mut self) {
        match self.journal.save().await {
            Ok(()) => {
                self.storage_retry_at = None;
                self.flush_events().await;
                self.send_command_result().await;
            }
            Err(error) => {
                if self.storage_retry_at.is_none() {
                    tracing::error!(%error, "saving the exam journal failed");
                }
                self.storage_retry_at = Some(Instant::now() + self.timing.retry);
            }
        }
    }

    fn publish(&self) {
        let next = ClientView {
            exam_id: self.exam_id,
            connected: self.connected,
            state: self.state,
            last_error_code: if self.storage_retry_at.is_some()
                || self.journal.has_unsendable_events()
            {
                Some(error_code::INTERNAL_ERROR.to_owned())
            } else {
                self.last_error_code.clone()
            },
        };
        self.view.send_if_modified(|view| {
            let changed = *view != next;
            *view = next.clone();
            changed
        });
    }

    fn next_deadline(&self) -> Option<Instant> {
        let command = self
            .command
            .as_ref()
            .filter(|command| !command.started)
            .map(|command| command.due);
        [
            self.reconnect_at,
            self.retry_at,
            self.ack_deadline,
            self.storage_retry_at,
            command,
            self.end_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Local deadline to stop recording when `end_exam` does not arrive.
    fn end_deadline(&self) -> Option<Instant> {
        if self.state.execution != ExecutionState::Recording {
            return None;
        }
        let (clock, end_at) = (self.clock?, self.end_at?);
        Some(clock.instant_of(end_at.saturating_add(END_GRACE)))
    }

    async fn tick(&mut self) {
        let now = Instant::now();
        if self.reconnect_at.is_some_and(|at| at <= now) {
            self.reconnect();
        }
        if self.ack_deadline.is_some_and(|at| at <= now) {
            tracing::warn!("command acknowledgement timed out; reconnecting");
            self.lose_connection(None);
        }
        if self.end_deadline().is_some_and(|at| at <= now) {
            self.stop_at_deadline().await;
        }
        if self.storage_retry_at.is_some_and(|at| at <= now) {
            self.persist().await;
        }
        if self.retry_at.is_some_and(|at| at <= now) {
            self.retry_at = None;
            self.try_start().await;
        }
        if self
            .command
            .as_ref()
            .is_some_and(|command| !command.started && command.due <= now)
        {
            self.execute().await;
        }
    }

    fn apply_cookie_check(&mut self, result: Result<AccodingCookies, ExamError>) {
        match result {
            Ok(cookies) => {
                self.cookies = cookies;
                if self.connected {
                    self.last_error_code = None;
                } else {
                    self.drop_link();
                    self.backoff = self.timing.reconnect_min;
                    self.reconnect_at = Some(Instant::now());
                }
            }
            Err(ExamError::Auth) => {
                self.last_error_code = Some(error_code::ACCODING_AUTH_EXPIRED.to_owned());
            }
            Err(ExamError::NotEnrolled) => {
                self.last_error_code = Some(error_code::NOT_ENROLLED.to_owned());
            }
            Err(error) => {
                tracing::warn!(%error, "checking updated Accoding cookies failed");
                self.last_error_code = Some(error_code::INTERNAL_ERROR.to_owned());
            }
        }
    }

    async fn navigation_denied(&mut self, target_origin: String) {
        if self.done || self.end_status().is_some() {
            return;
        }
        self.recent_denials
            .retain(|_, at| at.elapsed() < NAVIGATION_QUIET);
        if self.recent_denials.contains_key(&target_origin) {
            return;
        }
        let detail: String = target_origin.chars().take(MAX_DETAIL_CHARS).collect();
        self.recent_denials.insert(target_origin, Instant::now());
        self.incident(IncidentCode::NavigationDenied, Some(detail))
            .await;
    }

    async fn handle_inbound(&mut self, inbound: Inbound) {
        match inbound {
            Inbound::Opened(generation, link) if generation == self.generation => {
                let hello = hello(self.device_id, self.instance_id, self.state);
                if link.try_send(hello).is_ok() {
                    self.link = Some(link);
                } else {
                    self.lose_connection(None);
                }
            }
            Inbound::Message {
                generation,
                message,
                received_at,
            } if generation == self.generation => {
                self.handle_message(message, received_at).await;
            }
            Inbound::Closed(generation, error) if generation == self.generation => {
                self.lose_connection(error);
            }
            _ => {}
        }
    }

    async fn handle_message(&mut self, message: ServerMessage, received_at: Instant) {
        match message {
            ServerMessage::Sync {
                server_time,
                phase,
                end_at,
                ack_event_through,
                ..
            } => {
                self.clock = Some(Clock {
                    server_time,
                    // Recorder restoration and journal writes can delay handling.
                    at: received_at,
                });
                self.end_at = Some(end_at);
                self.connected = true;
                self.backoff = self.timing.reconnect_min;
                self.last_error_code = None;
                // Commands of the previous connection are superseded.
                self.command = None;
                self.ack_deadline = None;
                self.journal.anchor(ack_event_through);
                self.sent_through = ack_event_through;
                self.persist().await;
                if phase == ExamPhase::Running
                    && self.state.execution == ExecutionState::Recording
                    && self.state.recording == RecordingState::Failed
                    && self.retry_at.is_none()
                {
                    self.start_failures = 0;
                    self.retry_at = Some(Instant::now());
                }
            }
            ServerMessage::Command {
                command_id,
                name,
                effective_at,
            } => {
                if self
                    .command
                    .as_ref()
                    .is_some_and(|active| active.id == command_id)
                {
                    return;
                }
                // An acknowledgement for the previous command must not
                // delay the new command's result.
                self.ack_deadline = None;
                let due = self
                    .clock
                    .map_or_else(Instant::now, |clock| clock.instant_of(effective_at));
                self.command = Some(Active {
                    id: command_id,
                    name,
                    due,
                    started: false,
                    result: None,
                });
            }
            ServerMessage::CommandAck { command_id } => {
                if self.ack_deadline.is_none() {
                    return;
                }
                let Some(command) = self
                    .command
                    .take_if(|command| command.id == command_id && command.result.is_some())
                else {
                    return;
                };
                self.ack_deadline = None;
                if command.name == CommandName::EndExam {
                    self.complete().await;
                }
            }
            ServerMessage::Error { code, message } => {
                tracing::warn!(%code, %message, "monitor reported an error");
                self.last_error_code = Some(code);
            }
            _ => {}
        }
    }

    async fn recorder_event(&mut self, event: RecorderEvent) {
        if self.done
            || self.command.as_ref().is_some_and(|command| {
                command.name == CommandName::EndExam && command.result.is_some()
            })
        {
            // The end result is the last business message of this handshake.
            return;
        }
        match event {
            RecorderEvent::SegmentFinalized(meta) => {
                self.record(ExamEvent::SegmentFinalized(meta)).await;
            }
            RecorderEvent::Fault(error) => {
                if self.state.execution == ExecutionState::Recording
                    && self.state.recording == RecordingState::Failed
                {
                    return;
                }
                tracing::warn!(?error, "recording fault");
                self.incident(
                    IncidentCode::RecordingFailed,
                    Some(recording_detail(&error)),
                )
                .await;
                if self.state.execution == ExecutionState::Recording {
                    self.set_state(ExecutionState::Recording, RecordingState::Failed)
                        .await;
                    // The fault counts as the first failed attempt and is
                    // already reported.
                    self.start_failures = 1;
                    self.retry_at = Some(Instant::now() + self.timing.retry);
                }
            }
            _ => {}
        }
    }

    /// Executes the active command once it is due.
    async fn execute(&mut self) {
        let Some(command) = self.command.as_mut() else {
            return;
        };
        command.started = true;
        match command.name {
            CommandName::StartExam => match self.state.execution {
                ExecutionState::Ready | ExecutionState::Recording => {
                    if self.state.recording == RecordingState::Capturing {
                        self.resolve(CommandStatus::Completed, None).await;
                    } else {
                        // A capture already failing keeps its count, so the
                        // fault is not reported a second time.
                        if self.state.recording != RecordingState::Failed {
                            self.start_failures = 0;
                        }
                        self.retry_at = None;
                        self.try_start().await;
                    }
                }
                _ => {
                    let conflict = Some(error_code::COMMAND_STATE_CONFLICT.to_owned());
                    self.resolve(CommandStatus::Failed, conflict).await;
                }
            },
            CommandName::EndExam => {
                let status = match self.end_status() {
                    Some(status) => {
                        self.drain_recorder_events().await;
                        status
                    }
                    None => self.finish().await,
                };
                let code = (status == CommandStatus::Failed)
                    .then(|| error_code::RECORDING_FINISH_FAILED.to_owned());
                self.resolve(status, code).await;
            }
            _ => {}
        }
    }

    /// One attempt to start capture, scheduling the next one on failure.
    async fn try_start(&mut self) {
        let recovering = self.state.recording == RecordingState::Failed;
        match self.capture.start().await {
            Ok(()) => {
                self.start_failures = 0;
                self.set_state(ExecutionState::Recording, RecordingState::Capturing)
                    .await;
                if recovering {
                    self.incident(IncidentCode::RecordingRecovered, None).await;
                }
                self.resolve_start(CommandStatus::Completed, None).await;
            }
            Err(error) => {
                tracing::warn!(?error, "starting capture failed");
                if self.start_failures == 0 {
                    self.incident(
                        IncidentCode::RecordingFailed,
                        Some(recording_detail(&error)),
                    )
                    .await;
                }
                self.start_failures += 1;
                self.set_state(ExecutionState::Recording, RecordingState::Failed)
                    .await;
                let delay = if self.start_failures < self.timing.start_attempts {
                    self.timing.retry
                } else {
                    let code = Some(error_code::RECORDING_START_FAILED.to_owned());
                    self.resolve_start(CommandStatus::Failed, code).await;
                    // Losing the rest of the exam to a transient fault is worse
                    // than a slow retry, so capture is never given up.
                    self.timing.retry_max
                };
                self.retry_at = Some(Instant::now() + delay);
            }
        }
    }

    /// The journal records the outcome and its evidence together on success.
    fn end_status(&self) -> Option<CommandStatus> {
        match (self.state.execution, self.state.recording) {
            (ExecutionState::Finalizing, RecordingState::Complete) => {
                Some(CommandStatus::Completed)
            }
            (ExecutionState::Finalizing, RecordingState::Failed) => Some(CommandStatus::Failed),
            _ => None,
        }
    }

    /// Finishes the recording with bounded retries and reports the summary.
    async fn finish(&mut self) -> CommandStatus {
        self.retry_at = None;
        self.set_state(ExecutionState::Finalizing, RecordingState::Finalizing)
            .await;
        let mut attempt = 1;
        let result = loop {
            match self.finish_capture().await {
                Err(error)
                    if attempt < self.timing.finish_attempts
                        && !matches!(error, RecordingError::NoSegments) =>
                {
                    tracing::warn!(%error, attempt, "finishing the recording failed; retrying");
                    attempt += 1;
                    sleep(self.timing.retry).await;
                }
                result => break result,
            }
        };
        let result = result.and_then(|summary| {
            if self.journal.has_recording_segments(summary.segment_count) {
                Ok(summary)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "recorder segment events do not match the recording summary",
                )
                .into())
            }
        });
        match result {
            Ok(summary) => {
                let changed =
                    self.update_state(ExecutionState::Finalizing, RecordingState::Complete);
                self.record(ExamEvent::RecordingFinalized(summary)).await;
                if let Some(state) = changed {
                    self.announce(state).await;
                }
                CommandStatus::Completed
            }
            Err(error) => {
                let changed = self.update_state(ExecutionState::Finalizing, RecordingState::Failed);
                self.incident(
                    IncidentCode::RecordingFailed,
                    Some(recording_detail(&error)),
                )
                .await;
                if let Some(state) = changed {
                    self.announce(state).await;
                }
                CommandStatus::Failed
            }
        }
    }

    async fn finish_capture(&mut self) -> Result<RecordingSummary, RecordingError> {
        let mut received = Vec::new();
        let result = {
            // The recorder must deliver its tail segment before finish returns.
            // Keep reading while it waits for the bounded channel to have room.
            let finish = self.capture.finish();
            tokio::pin!(finish);
            loop {
                tokio::select! {
                    result = &mut finish => break result,
                    Some(event) = self.recorder.recv() => received.push(event),
                }
            }
        };
        for event in received {
            self.recorder_event(event).await;
        }
        self.drain_recorder_events().await;
        result
    }

    async fn drain_recorder_events(&mut self) {
        while let Ok(event) = self.recorder.try_recv() {
            self.recorder_event(event).await;
        }
    }

    /// Stops recording after `end_at + 60s` passed without `end_exam`.
    async fn stop_at_deadline(&mut self) {
        tracing::warn!("end_exam did not arrive in time; stopping the recording locally");
        self.retry_at = None;
        if let Err(error) = self.capture.stop().await {
            self.incident(
                IncidentCode::RecordingFailed,
                Some(recording_detail(&error)),
            )
            .await;
        }
        self.set_state(ExecutionState::Finalizing, RecordingState::Finalizing)
            .await;
    }

    /// Completes the end handshake after the server acknowledged `end_exam`.
    async fn complete(&mut self) {
        let recording = if self.state.recording == RecordingState::Complete {
            RecordingState::Complete
        } else {
            RecordingState::Failed
        };
        self.done = true;
        self.reconnect_at = None;
        self.drop_link();
        self.journal.clear_events();
        self.set_state(ExecutionState::Finished, recording).await;
    }

    /// Fixes the result of a pending `start_exam`, if one is waiting.
    async fn resolve_start(&mut self, status: CommandStatus, error_code: Option<String>) {
        if self.command.as_ref().is_some_and(|command| {
            command.name == CommandName::StartExam && command.started && command.result.is_none()
        }) {
            self.resolve(status, error_code).await;
        }
    }

    /// Records the result of the active command and reports it.
    async fn resolve(&mut self, status: CommandStatus, error_code: Option<String>) {
        let Some(command) = self.command.as_mut() else {
            return;
        };
        command.result = Some((status, error_code));
        self.send_command_result().await;
    }

    async fn send_command_result(&mut self) {
        let Some(command) = &self.command else {
            return;
        };
        // The server can complete the exam as soon as it receives the end
        // result. Wait for the journal to release all preceding evidence.
        if !self.connected
            || self.ack_deadline.is_some()
            || (command.name == CommandName::EndExam
                && (self.storage_retry_at.is_some() || self.journal.has_unsendable_events()))
        {
            return;
        }
        let Some((status, error_code)) = &command.result else {
            return;
        };
        let message = ClientMessage::CommandResult {
            command_id: command.id,
            status: *status,
            error_code: error_code.clone(),
        };
        self.send(message).await;
        if self.connected {
            self.ack_deadline = Some(Instant::now() + self.timing.ack_timeout);
        }
    }

    async fn incident(&mut self, code: IncidentCode, detail: Option<String>) {
        let occurred_at = self
            .clock
            .map_or_else(OffsetDateTime::now_utc, |clock| clock.now());
        self.record(ExamEvent::Incident(ClientIncident {
            code,
            occurred_at,
            detail,
        }))
        .await;
    }

    /// Persists an event before sending it.
    async fn record(&mut self, event: ExamEvent) {
        self.journal.append(event);
        self.persist().await;
    }

    async fn set_state(&mut self, execution: ExecutionState, recording: RecordingState) {
        if let Some(state) = self.update_state(execution, recording) {
            self.persist().await;
            self.announce(state).await;
        }
    }

    /// Records a new state in memory and in the journal without saving it;
    /// returns the state when it changed.
    fn update_state(
        &mut self,
        execution: ExecutionState,
        recording: RecordingState,
    ) -> Option<ClientState> {
        let state = ClientState {
            execution,
            recording,
        };
        if state == self.state {
            return None;
        }
        self.state = state;
        self.journal.set_state(state);
        Some(state)
    }

    /// Reports a saved state change to the server and the UI.
    async fn announce(&mut self, state: ClientState) {
        self.send(ClientMessage::Status { state }).await;
        self.publish();
    }

    /// Sends a message on a synchronized connection; otherwise it is dropped,
    /// because the journal or the next `hello` carries it after reconnecting.
    async fn send(&mut self, message: ClientMessage) {
        if !self.connected {
            return;
        }
        let Some(link) = &self.link else {
            return;
        };
        if !matches!(
            timeout_at(self.send_deadline(), link.send(message)).await,
            Ok(Ok(()))
        ) {
            self.lose_connection(None);
        }
    }

    /// The whole replay shares one budget so a long queue cannot monopolize
    /// the runtime even when the socket occasionally makes progress.
    async fn flush_events(&mut self) {
        if !self.connected {
            return;
        }
        let Some(link) = &self.link else {
            return;
        };
        let deadline = self.send_deadline();
        let result = timeout_at(deadline, async {
            for (seq, message) in self.journal.messages_after(self.sent_through) {
                link.send(message).await?;
                self.sent_through = seq;
            }
            Ok::<_, mpsc::error::SendError<ClientMessage>>(())
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            self.lose_connection(None);
        }
    }

    fn send_deadline(&self) -> Instant {
        let budget = Instant::now() + self.timing.liveness.heartbeat;
        self.end_deadline().map_or(budget, |end| budget.min(end))
    }

    fn drop_link(&mut self) {
        self.link = None;
        self.session = None;
        self.connected = false;
        self.ack_deadline = None;
        // Reports of the closed session are ignored from now on.
        self.generation += 1;
    }

    fn lose_connection(&mut self, error: Option<ExamError>) {
        self.drop_link();
        if self.done {
            return;
        }
        match error {
            Some(ExamError::Auth) => {
                self.last_error_code = Some(error_code::ACCODING_AUTH_EXPIRED.to_owned());
            }
            Some(ExamError::NotEnrolled) => {
                self.last_error_code = Some(error_code::NOT_ENROLLED.to_owned());
            }
            Some(error) => tracing::warn!(%error, "monitor connection lost"),
            None => {}
        }
        let delay = jittered_backoff(self.backoff, random_u64());
        self.reconnect_at = Some(Instant::now() + delay);
        self.backoff = (self.backoff * 2).min(self.timing.reconnect_max);
    }

    fn reconnect(&mut self) {
        self.reconnect_at = None;
        self.generation += 1;
        match self.monitor.socket_request(&self.cookies, self.exam_id) {
            Ok(request) => {
                self.session = Some(session::spawn(
                    request,
                    self.generation,
                    self.inbound_tx.clone(),
                    self.timing.liveness,
                ));
            }
            Err(error) => self.lose_connection(Some(error)),
        }
    }
}

fn recording_detail(error: &RecordingError) -> String {
    // Platform failures carry the capture operation and native error code.
    // Keep filesystem paths out of the remotely reported storage errors.
    let detail = match error {
        RecordingError::Platform(Some(source)) => format!("{error}: {source}"),
        _ => error.to_string(),
    };
    detail.chars().take(MAX_DETAIL_CHARS).collect()
}

async fn sleep_until_some(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn random_u64() -> u64 {
    RandomState::new().build_hasher().finish()
}

fn jittered_backoff(backoff: Duration, sample: u64) -> Duration {
    let half = backoff / 2;
    let span_millis = u64::try_from(backoff.saturating_sub(half).as_millis()).unwrap_or(u64::MAX);
    if span_millis == 0 {
        return backoff;
    }
    half + Duration::from_millis(sample % (span_millis + 1))
}

#[cfg(test)]
mod tests;
