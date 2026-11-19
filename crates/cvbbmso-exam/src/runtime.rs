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

use std::{future::Future, path::PathBuf, time::Duration};

use cvbbmso_accoding::AccodingCookies;
use cvbbmso_protocol::{
    ClientHello, ClientIncident, ClientMessage, ClientState, CommandName, CommandStatus, ExamEvent,
    ExamPhase, ExecutionState, IncidentCode, Platform, RecordingPolicy, RecordingState,
    ServerMessage, error_code,
};
use cvbbmso_recorder::{RecorderEvent, RecordingError};
use time::OffsetDateTime;
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::{Instant, sleep, sleep_until, timeout},
};
use uuid::Uuid;

pub(crate) use capture::Capture;
use session::{Inbound, Liveness};

use crate::{
    ClientView, ExamConfig, ExamError, ExamHandle, UiNotice, journal::Journal, monitor::Monitor,
};

/// Grace period after `end_at` before an offline client stops recording on
/// its own, fixed by the design contract.
const END_GRACE: Duration = Duration::from_secs(60);
/// Longest detail kept for a navigation incident.
const MAX_DETAIL_CHARS: usize = 200;
/// Repeated denials of one origin within this window are reported once.
const NAVIGATION_QUIET: Duration = Duration::from_secs(5);

/// Durations and retry bounds of the runtime.
///
/// Production uses [`Timing::default`]; tests shorten the waits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// Heartbeat cadence.
    pub(crate) heartbeat: Duration,
    /// Silence after which a connection counts as dead; also bounds the
    /// handshake and the wait for the first `sync`.
    pub(crate) silence: Duration,
    /// First reconnect delay; doubled after each failure.
    pub(crate) reconnect_min: Duration,
    /// Longest reconnect delay.
    pub(crate) reconnect_max: Duration,
    /// Delay between recorder attempts.
    pub(crate) retry: Duration,
    /// Attempts to start capture before `start_exam` fails.
    pub(crate) start_attempts: u32,
    /// Attempts to finish the recording before `end_exam` fails.
    pub(crate) finish_attempts: u32,
    /// Wait for `command_ack` before the connection is replaced.
    pub(crate) ack_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(5),
            silence: Duration::from_secs(20),
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(30),
            retry: Duration::from_secs(3),
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
    pub(crate) oj_user_id: i64,
    pub(crate) cookies: AccodingCookies,
    pub(crate) monitor: Monitor,
    pub(crate) timing: Timing,
}

/// Opens the journal, connects, waits for the first `sync`, opens the
/// recorder, and starts the runtime task.
pub(crate) async fn launch<C, F, Fut>(launch: Launch, open: F) -> Result<ExamHandle, ExamError>
where
    C: Capture,
    F: FnOnce(PathBuf, RecordingPolicy) -> Fut,
    Fut: Future<Output = Result<(C, mpsc::Receiver<RecorderEvent>), RecordingError>>,
{
    let Launch {
        config,
        exam_id,
        oj_user_id,
        cookies,
        monitor,
        timing,
    } = launch;
    let candidate_dir = config
        .app_data_dir
        .join("exams")
        .join(exam_id.to_string())
        .join(oj_user_id.to_string());
    let journal = Journal::open(&candidate_dir).await?;
    let state = resumed(journal.state());

    let (inbound_tx, mut inbound) = mpsc::channel(64);
    let liveness = Liveness {
        heartbeat: timing.heartbeat,
        silence: timing.silence,
    };
    session::spawn(
        monitor.socket_request(&cookies, exam_id)?,
        1,
        inbound_tx.clone(),
        liveness,
    );
    let hello = ClientMessage::Hello(ClientHello {
        device_id: config.device_id,
        instance_id: config.instance_id,
        platform: platform(),
        state,
    });
    let (link, sync) = timeout(timing.silence, first_sync(&mut inbound, hello))
        .await
        .map_err(|_| ExamError::WebSocket(None))??;
    let ServerMessage::Sync {
        recording_policy, ..
    } = &sync
    else {
        return Err(ExamError::WebSocket(None));
    };
    let (capture, recorder) = open(candidate_dir, *recording_policy)
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
        device_id: config.device_id,
        instance_id: config.instance_id,
        monitor,
        cookies,
        timing,
        liveness,
        journal,
        capture,
        recorder,
        inbound_tx,
        inbound,
        requests,
        view: view_tx,
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
        end_outcome: None,
        ack_deadline: None,
        last_denial: None,
        done: false,
        fatal: None,
    };
    runtime.handle_message(sync).await?;
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
) -> Result<(mpsc::Sender<ClientMessage>, ServerMessage), ExamError> {
    let link = match inbound.recv().await {
        Some(Inbound::Opened(_, link)) => link,
        Some(Inbound::Closed(_, error)) => return Err(error.unwrap_or(ExamError::WebSocket(None))),
        _ => return Err(ExamError::WebSocket(None)),
    };
    link.send(hello)
        .await
        .map_err(|_| ExamError::WebSocket(None))?;
    match inbound.recv().await {
        Some(Inbound::Message(_, sync @ ServerMessage::Sync { .. })) => Ok((link, sync)),
        Some(Inbound::Message(_, ServerMessage::Error { code, .. })) => Err(code_error(&code)),
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
        self.server_time + self.at.elapsed()
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
    device_id: Uuid,
    instance_id: Uuid,
    monitor: Monitor,
    cookies: AccodingCookies,
    timing: Timing,
    liveness: Liveness,
    journal: Journal,
    capture: C,
    recorder: mpsc::Receiver<RecorderEvent>,
    inbound_tx: mpsc::Sender<Inbound>,
    inbound: mpsc::Receiver<Inbound>,
    requests: mpsc::Receiver<Request>,
    view: watch::Sender<ClientView>,
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
    /// Result of finishing the recording, reused when `end_exam` repeats.
    end_outcome: Option<(CommandStatus, Option<String>)>,
    ack_deadline: Option<Instant>,
    last_denial: Option<(String, Instant)>,
    /// The end handshake completed; the runtime no longer reconnects.
    done: bool,
    /// Unrecoverable local failure that stopped the runtime.
    fatal: Option<ExamError>,
}

impl<C: Capture> Runtime<C> {
    async fn run(mut self) {
        loop {
            self.publish();
            let deadline = self.next_deadline();
            let outcome = tokio::select! {
                request = self.requests.recv() => match request {
                    Some(Request::Notice(notice)) => self.notice(notice).await,
                    Some(Request::Shutdown(reply)) => {
                        let _ = reply.send(self.stop().await);
                        return;
                    }
                    None => {
                        let _ = self.stop().await;
                        return;
                    }
                },
                Some(inbound) = self.inbound.recv() => self.handle_inbound(inbound).await,
                Some(event) = self.recorder.recv() => self.recorder_event(event).await,
                () = sleep_until_some(deadline) => self.tick().await,
            };
            if let Err(error) = outcome {
                self.fail(error).await;
                self.serve_stopped().await;
                return;
            }
        }
    }

    /// Stops after an unrecoverable local failure and reports it to the UI.
    async fn fail(&mut self, error: ExamError) {
        tracing::error!(%error, "exam runtime stopped");
        let _ = self.capture.stop().await;
        self.drop_link();
        self.last_error_code = Some(error_code::INTERNAL_ERROR.to_owned());
        self.fatal = Some(error);
        self.publish();
    }

    /// Answers requests after a fatal failure until the handle shuts down.
    async fn serve_stopped(mut self) {
        while let Some(request) = self.requests.recv().await {
            if let Request::Shutdown(reply) = request {
                let _ = reply.send(self.fatal.take().map_or(Ok(()), Err));
                return;
            }
        }
    }

    async fn stop(&mut self) -> Result<(), ExamError> {
        self.drop_link();
        self.capture.stop().await.map_err(ExamError::Recording)?;
        self.journal.save().await
    }

    fn publish(&self) {
        let next = ClientView {
            exam_id: self.exam_id,
            connected: self.connected,
            state: self.state,
            last_error_code: self.last_error_code.clone(),
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
        Some(clock.instant_of(end_at) + END_GRACE)
    }

    async fn tick(&mut self) -> Result<(), ExamError> {
        let now = Instant::now();
        if self.reconnect_at.is_some_and(|at| at <= now) {
            self.reconnect();
        }
        if self.ack_deadline.is_some_and(|at| at <= now) {
            tracing::warn!("command acknowledgement timed out; reconnecting");
            self.lose_connection(None);
        }
        if self.end_deadline().is_some_and(|at| at <= now) {
            self.stop_at_deadline().await?;
        }
        if self.retry_at.is_some_and(|at| at <= now) {
            self.retry_at = None;
            self.try_start().await?;
        }
        if self
            .command
            .as_ref()
            .is_some_and(|command| !command.started && command.due <= now)
        {
            self.execute().await?;
        }
        Ok(())
    }

    async fn notice(&mut self, notice: UiNotice) -> Result<(), ExamError> {
        match notice {
            UiNotice::CookiesUpdated(cookies) => {
                self.cookies = cookies;
                if self.reconnect_at.is_some() {
                    self.backoff = self.timing.reconnect_min;
                    self.reconnect_at = Some(Instant::now());
                }
                Ok(())
            }
            UiNotice::NavigationDenied { target_origin } => {
                let detail: String = target_origin.chars().take(MAX_DETAIL_CHARS).collect();
                let repeated = self.last_denial.as_ref().is_some_and(|(origin, at)| {
                    *origin == detail && at.elapsed() < NAVIGATION_QUIET
                });
                self.last_denial = Some((detail.clone(), Instant::now()));
                if repeated {
                    return Ok(());
                }
                self.incident(IncidentCode::NavigationDenied, Some(detail))
                    .await
            }
        }
    }

    async fn handle_inbound(&mut self, inbound: Inbound) -> Result<(), ExamError> {
        match inbound {
            Inbound::Opened(generation, link) if generation == self.generation => {
                let hello = ClientMessage::Hello(ClientHello {
                    device_id: self.device_id,
                    instance_id: self.instance_id,
                    platform: platform(),
                    state: self.state,
                });
                if link.send(hello).await.is_ok() {
                    self.link = Some(link);
                } else {
                    self.lose_connection(None);
                }
                Ok(())
            }
            Inbound::Message(generation, message) if generation == self.generation => {
                self.handle_message(message).await
            }
            Inbound::Closed(generation, error) if generation == self.generation => {
                self.lose_connection(error);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn handle_message(&mut self, message: ServerMessage) -> Result<(), ExamError> {
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
                    at: Instant::now(),
                });
                self.end_at = Some(end_at);
                self.connected = true;
                self.backoff = self.timing.reconnect_min;
                self.last_error_code = None;
                // Commands of the previous connection are superseded.
                self.command = None;
                self.ack_deadline = None;
                let resend = self.journal.anchor(ack_event_through);
                self.journal.save().await?;
                for message in resend {
                    self.send(message).await;
                }
                if phase == ExamPhase::Running
                    && self.state.execution == ExecutionState::Recording
                    && self.state.recording == RecordingState::Failed
                    && self.retry_at.is_none()
                {
                    self.start_failures = 0;
                    self.retry_at = Some(Instant::now());
                }
                Ok(())
            }
            ServerMessage::Command {
                command_id,
                name,
                effective_at,
            } => {
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
                Ok(())
            }
            ServerMessage::CommandAck { command_id } => {
                let Some(command) = self
                    .command
                    .take_if(|command| command.id == command_id && command.result.is_some())
                else {
                    return Ok(());
                };
                self.ack_deadline = None;
                if command.name == CommandName::EndExam {
                    self.complete().await?;
                }
                Ok(())
            }
            ServerMessage::Error { code, message } => {
                tracing::warn!(%code, %message, "monitor reported an error");
                self.last_error_code = Some(code);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn recorder_event(&mut self, event: RecorderEvent) -> Result<(), ExamError> {
        match event {
            RecorderEvent::SegmentFinalized(meta) => {
                self.record(ExamEvent::SegmentFinalized(meta)).await
            }
            RecorderEvent::Fault(error) => {
                tracing::warn!(%error, "recording fault");
                self.incident(IncidentCode::RecordingFailed, Some(error.to_string()))
                    .await?;
                if self.state.execution == ExecutionState::Recording {
                    self.set_state(ExecutionState::Recording, RecordingState::Failed)
                        .await?;
                    // The fault counts as the first failed attempt and is
                    // already reported.
                    self.start_failures = 1;
                    self.retry_at = Some(Instant::now() + self.timing.retry);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Executes the active command once it is due.
    async fn execute(&mut self) -> Result<(), ExamError> {
        let Some(command) = self.command.as_mut() else {
            return Ok(());
        };
        command.started = true;
        match command.name {
            CommandName::StartExam => match self.state.execution {
                ExecutionState::Ready | ExecutionState::Recording => {
                    if self.state.recording == RecordingState::Capturing {
                        self.resolve(CommandStatus::Completed, None).await;
                        Ok(())
                    } else {
                        self.start_failures = 0;
                        self.retry_at = None;
                        self.try_start().await
                    }
                }
                _ => {
                    let conflict = Some(error_code::COMMAND_STATE_CONFLICT.to_owned());
                    self.resolve(CommandStatus::Failed, conflict).await;
                    Ok(())
                }
            },
            CommandName::EndExam => {
                let outcome = match self.end_outcome.clone() {
                    Some(outcome) => outcome,
                    None => self.finish().await?,
                };
                self.resolve(outcome.0, outcome.1).await;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// One attempt to start capture, scheduling the next one on failure.
    async fn try_start(&mut self) -> Result<(), ExamError> {
        let recovering = self.state.recording == RecordingState::Failed;
        match self.capture.start().await {
            Ok(()) => {
                self.start_failures = 0;
                self.set_state(ExecutionState::Recording, RecordingState::Capturing)
                    .await?;
                if recovering {
                    self.incident(IncidentCode::RecordingRecovered, None)
                        .await?;
                }
                self.resolve_start(CommandStatus::Completed, None).await;
            }
            Err(error) => {
                tracing::warn!(%error, "starting capture failed");
                if self.start_failures == 0 {
                    self.incident(IncidentCode::RecordingFailed, Some(error.to_string()))
                        .await?;
                }
                self.start_failures += 1;
                self.set_state(ExecutionState::Recording, RecordingState::Failed)
                    .await?;
                if self.start_failures < self.timing.start_attempts {
                    self.retry_at = Some(Instant::now() + self.timing.retry);
                } else {
                    let code = Some(error_code::RECORDING_START_FAILED.to_owned());
                    self.resolve_start(CommandStatus::Failed, code).await;
                }
            }
        }
        Ok(())
    }

    /// Finishes the recording with bounded retries and reports the manifest.
    async fn finish(&mut self) -> Result<(CommandStatus, Option<String>), ExamError> {
        self.retry_at = None;
        self.set_state(ExecutionState::Finalizing, RecordingState::Finalizing)
            .await?;
        let mut attempt = 1;
        let outcome = loop {
            match self.capture.finish().await {
                Ok(manifest) => {
                    self.record(ExamEvent::RecordingFinalized(manifest)).await?;
                    self.set_state(ExecutionState::Finalizing, RecordingState::Complete)
                        .await?;
                    break (CommandStatus::Completed, None);
                }
                Err(error)
                    if attempt < self.timing.finish_attempts
                        && !matches!(error, RecordingError::NoSegments) =>
                {
                    tracing::warn!(%error, attempt, "finishing the recording failed; retrying");
                    attempt += 1;
                    sleep(self.timing.retry).await;
                }
                Err(error) => {
                    self.incident(IncidentCode::RecordingFailed, Some(error.to_string()))
                        .await?;
                    self.set_state(ExecutionState::Finalizing, RecordingState::Failed)
                        .await?;
                    break (
                        CommandStatus::Failed,
                        Some(error_code::RECORDING_FINISH_FAILED.to_owned()),
                    );
                }
            }
        };
        self.end_outcome = Some(outcome.clone());
        Ok(outcome)
    }

    /// Stops recording after `end_at + 60s` passed without `end_exam`.
    async fn stop_at_deadline(&mut self) -> Result<(), ExamError> {
        tracing::warn!("end_exam did not arrive in time; stopping the recording locally");
        self.retry_at = None;
        if let Err(error) = self.capture.stop().await {
            self.incident(IncidentCode::RecordingFailed, Some(error.to_string()))
                .await?;
        }
        self.set_state(ExecutionState::Finalizing, RecordingState::Finalizing)
            .await
    }

    /// Completes the end handshake after the server acknowledged `end_exam`.
    async fn complete(&mut self) -> Result<(), ExamError> {
        let recording = if self.state.recording == RecordingState::Complete {
            RecordingState::Complete
        } else {
            RecordingState::Failed
        };
        self.journal.clear_events();
        self.set_state(ExecutionState::Finished, recording).await?;
        self.done = true;
        self.reconnect_at = None;
        self.drop_link();
        Ok(())
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
        command.result = Some((status, error_code.clone()));
        let message = ClientMessage::CommandResult {
            command_id: command.id,
            status,
            error_code,
        };
        if self.connected {
            self.ack_deadline = Some(Instant::now() + self.timing.ack_timeout);
        }
        self.send(message).await;
    }

    async fn incident(
        &mut self,
        code: IncidentCode,
        detail: Option<String>,
    ) -> Result<(), ExamError> {
        let occurred_at = self
            .clock
            .map_or_else(OffsetDateTime::now_utc, |clock| clock.now());
        self.record(ExamEvent::Incident(ClientIncident {
            code,
            occurred_at,
            detail,
        }))
        .await
    }

    /// Persists an event before sending it.
    async fn record(&mut self, event: ExamEvent) -> Result<(), ExamError> {
        let message = self.journal.append(event);
        self.journal.save().await?;
        if let Some(message) = message {
            self.send(message).await;
        }
        Ok(())
    }

    async fn set_state(
        &mut self,
        execution: ExecutionState,
        recording: RecordingState,
    ) -> Result<(), ExamError> {
        let state = ClientState {
            execution,
            recording,
        };
        if state == self.state {
            return Ok(());
        }
        self.state = state;
        self.journal.set_state(state);
        self.journal.save().await?;
        self.send(ClientMessage::Status { state }).await;
        self.publish();
        Ok(())
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
        if link.send(message).await.is_err() {
            self.lose_connection(None);
        }
    }

    fn drop_link(&mut self) {
        self.link = None;
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
        self.reconnect_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(self.timing.reconnect_max);
    }

    fn reconnect(&mut self) {
        self.reconnect_at = None;
        self.generation += 1;
        match self.monitor.socket_request(&self.cookies, self.exam_id) {
            Ok(request) => session::spawn(
                request,
                self.generation,
                self.inbound_tx.clone(),
                self.liveness,
            ),
            Err(error) => self.lose_connection(Some(error)),
        }
    }
}

async fn sleep_until_some(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests;
