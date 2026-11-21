use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque, hash_map::Entry},
    sync::Arc,
    time::Duration,
};

use cvbbmso_domain::{DisconnectReason, DomainError, ExamService};
use cvbbmso_protocol::{
    ClientHello, ClientMessage, CommandName, CommandStatus, ServerMessage, error_code,
};
use time::OffsetDateTime;
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, MissedTickBehavior, interval, sleep_until},
};
use uuid::Uuid;

const INBOUND_CAPACITY: usize = 256;
pub(crate) const OUTBOUND_CAPACITY: usize = 32;
const SCHEDULE_INTERVAL: Duration = Duration::from_secs(1);
const DISCONNECT_RETRY_BATCH: usize = 16;
const WRITE_ATTEMPTS: u8 = 4;
const WRITE_RETRY_DELAY: Duration = Duration::from_millis(50);

enum ReceiveOutcome {
    Continue(Instant),
    Retry(ClientMessage, Instant),
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct StudentKey {
    pub(crate) exam_id: Uuid,
    pub(crate) oj_user_id: i64,
}

#[derive(Clone, Copy)]
pub(crate) struct Connection {
    pub(crate) student: StudentKey,
    pub(crate) id: Uuid,
    pub(crate) request_id: Uuid,
}

#[derive(Clone)]
pub(crate) struct Hub(mpsc::Sender<Request>);

pub(crate) struct Peer {
    pub(crate) frames: mpsc::Sender<ServerMessage>,
    pub(crate) close: oneshot::Sender<Option<SocketError>>,
    pub(crate) admitted: oneshot::Sender<Instant>,
}

enum Request {
    Connect {
        connection: Connection,
        hello: ClientHello,
        peer: Peer,
    },
    Receive {
        connection: Connection,
        message: ClientMessage,
        received_at: Instant,
        attempt: u8,
        processed: oneshot::Sender<ReceiveOutcome>,
    },
    End {
        connection: Connection,
        reason: DisconnectReason,
        error: Option<SocketError>,
    },
    Schedule,
    #[cfg(test)]
    Barrier(oneshot::Sender<()>),
}

impl Hub {
    pub(crate) async fn connect(
        &self,
        connection: Connection,
        hello: ClientHello,
        peer: Peer,
    ) -> bool {
        self.0
            .send(Request::Connect {
                connection,
                hello,
                peer,
            })
            .await
            .is_ok()
    }

    pub(crate) async fn receive(
        &self,
        connection: Connection,
        mut message: ClientMessage,
        received_at: Instant,
        on_retry: impl FnOnce(),
    ) -> Option<Instant> {
        let mut on_retry = Some(on_retry);
        // Wait outside the manager between attempts so other peers can make progress.
        for attempt in 1..=WRITE_ATTEMPTS {
            let (processed, completion) = oneshot::channel();
            self.0
                .send(Request::Receive {
                    connection,
                    message,
                    received_at,
                    attempt,
                    processed,
                })
                .await
                .ok()?;
            match completion.await.ok()? {
                ReceiveOutcome::Continue(deadline) => return Some(deadline),
                ReceiveOutcome::Stop => return None,
                ReceiveOutcome::Retry(retry, deadline) => {
                    if let Some(on_retry) = on_retry.take() {
                        on_retry();
                    }
                    message = retry;
                    sleep_until((Instant::now() + WRITE_RETRY_DELAY).min(deadline)).await;
                }
            }
        }
        None
    }

    pub(crate) async fn end(
        &self,
        connection: Connection,
        reason: DisconnectReason,
        error: Option<SocketError>,
    ) {
        let _ = self
            .0
            .send(Request::End {
                connection,
                reason,
                error,
            })
            .await;
    }

    #[cfg(test)]
    pub(crate) async fn trigger_schedule(&self) {
        assert!(self.0.send(Request::Schedule).await.is_ok());
    }

    #[cfg(test)]
    pub(crate) async fn barrier(&self) {
        let (send, receive) = oneshot::channel();
        assert!(self.0.send(Request::Barrier(send)).await.is_ok());
        receive.await.unwrap();
    }

    pub(crate) async fn schedule(self) {
        let mut ticks = interval(SCHEDULE_INTERVAL);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            if self.0.send(Request::Schedule).await.is_err() {
                break;
            }
        }
    }
}

struct Current {
    connection: Connection,
    deadline: Instant,
    frames: mpsc::Sender<ServerMessage>,
    close: oneshot::Sender<Option<SocketError>>,
    start: Option<Command>,
    end: Option<Command>,
}

struct Command {
    id: Uuid,
    name: CommandName,
    result: Option<(CommandStatus, Option<String>)>,
}

impl Current {
    fn send(&mut self, frame: ServerMessage) -> bool {
        if let ServerMessage::Command {
            command_id, name, ..
        } = frame
        {
            let slot = match name {
                CommandName::StartExam => &mut self.start,
                CommandName::EndExam => &mut self.end,
                _ => return false,
            };
            if slot.is_some() {
                return true;
            }
            *slot = Some(Command {
                id: command_id,
                name,
                result: None,
            });
        }
        self.frames.try_send(frame).is_ok()
    }

    async fn receive(
        &mut self,
        exam: &ExamService,
        message: ClientMessage,
    ) -> Result<bool, DomainError> {
        let StudentKey {
            exam_id,
            oj_user_id,
        } = self.connection.student;
        let ClientMessage::CommandResult {
            command_id,
            status,
            error_code,
        } = message
        else {
            exam.receive(exam_id, oj_user_id, message).await?;
            return Ok(true);
        };

        let command = [&mut self.start, &mut self.end]
            .into_iter()
            .filter_map(Option::as_mut)
            .find(|command| command.id == command_id)
            .ok_or(DomainError::Conflict)?;
        let result = (status, error_code);
        match &command.result {
            Some(previous) if previous != &result => return Err(DomainError::Conflict),
            Some(_) => exam.check_connected(exam_id, oj_user_id).await?,
            None => {
                exam.record_command_result(
                    exam_id,
                    oj_user_id,
                    command_id,
                    command.name,
                    status,
                    result.1.clone(),
                )
                .await?;
                command.result = Some(result);
            }
        }
        Ok(self
            .frames
            .try_send(ServerMessage::CommandAck { command_id })
            .is_ok())
    }
}

#[derive(Clone, Copy)]
struct LostConnection {
    connection: Connection,
    reason: DisconnectReason,
    observed_at: OffsetDateTime,
}

pub(crate) struct Manager {
    exam: Arc<ExamService>,
    requests: mpsc::Receiver<Request>,
    current: HashMap<StudentKey, Current>,
    lost: VecDeque<LostConnection>,
    timeout: Duration,
    #[cfg(test)]
    pub(crate) write_retries: Arc<tokio::sync::Notify>,
}

impl Manager {
    pub(crate) fn new(exam: Arc<ExamService>, timeout: Duration) -> (Hub, Self) {
        let (sender, requests) = mpsc::channel(INBOUND_CAPACITY);
        (
            Hub(sender),
            Self {
                exam,
                requests,
                current: HashMap::new(),
                lost: VecDeque::new(),
                timeout,
                #[cfg(test)]
                write_retries: Arc::default(),
            },
        )
    }

    pub(crate) async fn run(mut self) {
        while let Some(request) = self.requests.recv().await {
            match request {
                Request::Connect {
                    connection,
                    hello,
                    peer,
                } => {
                    self.connect(connection, hello, peer).await;
                }
                Request::Receive {
                    connection,
                    message,
                    received_at,
                    attempt,
                    processed,
                } => {
                    // Let an active transaction settle before admitting a replacement.
                    // Requests still in the queue can be discarded when the reader ends.
                    if !processed.is_closed() {
                        let outcome = self
                            .receive(connection, message, received_at, attempt)
                            .await;
                        let _ = processed.send(outcome);
                    }
                }
                Request::End {
                    connection,
                    reason,
                    error,
                } => {
                    self.end(connection, reason, error).await;
                }
                Request::Schedule => {
                    self.dispatch().await;
                    self.retry_lost().await;
                }
                #[cfg(test)]
                Request::Barrier(done) => {
                    let _ = done.send(());
                }
            }
        }
    }

    async fn connect(&mut self, connection: Connection, hello: ClientHello, peer: Peer) {
        // Commit the previous socket's loss before marking this student connected again.
        if let Some(index) = self
            .lost
            .iter()
            .position(|lost| lost.connection.student == connection.student)
            && let Some(lost) = self.lost.remove(index)
            && !self.record_lost(lost).await
        {
            connection.span().in_scope(|| {
                tracing::warn!("WebSocket admission deferred until disconnect is recorded");
            });
            let _ = peer.close.send(Some(SocketError::INTERNAL_ERROR));
            return;
        }
        let StudentKey {
            exam_id,
            oj_user_id,
        } = connection.student;
        let span = connection.span();
        let outcome = match self.exam.connect(exam_id, oj_user_id, hello).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let error = SocketError::domain(&error);
                span.in_scope(|| error.log_rejection("WebSocket admission rejected"));
                let _ = peer.close.send(Some(error));
                return;
            }
        };

        let deadline = Instant::now() + self.timeout;
        let mut current = Current {
            connection,
            deadline,
            frames: peer.frames,
            close: peer.close,
            start: None,
            end: None,
        };
        let delivered = std::iter::once(outcome.sync)
            .chain(outcome.commands)
            .all(|frame| current.send(frame));
        if let Some(previous) = self.current.insert(connection.student, current) {
            let _ = previous.close.send(None);
        }
        span.in_scope(|| tracing::info!("WebSocket admitted"));
        if !delivered || peer.admitted.send(deadline).is_err() {
            self.end(connection, DisconnectReason::Closed, None).await;
        }
    }

    async fn receive(
        &mut self,
        connection: Connection,
        message: ClientMessage,
        received_at: Instant,
        attempt: u8,
    ) -> ReceiveOutcome {
        let Some(current) = self
            .current
            .get_mut(&connection.student)
            .filter(|current| current.connection.id == connection.id)
        else {
            return ReceiveOutcome::Stop;
        };
        if received_at >= current.deadline || (attempt > 1 && Instant::now() >= current.deadline) {
            self.end(connection, DisconnectReason::HeartbeatTimeout, None)
                .await;
            return ReceiveOutcome::Stop;
        }
        let heartbeat = matches!(message, ClientMessage::Heartbeat);
        match current.receive(&self.exam, message.clone()).await {
            Ok(true) => {
                if heartbeat {
                    current.deadline = current.deadline.max(received_at + self.timeout);
                }
                return ReceiveOutcome::Continue(current.deadline);
            }
            Ok(false) => self.end(connection, DisconnectReason::Closed, None).await,
            Err(error) => {
                if error.is_storage_busy() {
                    if Instant::now() >= current.deadline {
                        self.end(connection, DisconnectReason::HeartbeatTimeout, None)
                            .await;
                        return ReceiveOutcome::Stop;
                    }
                    if attempt < WRITE_ATTEMPTS {
                        #[cfg(test)]
                        self.write_retries.notify_one();
                        return ReceiveOutcome::Retry(message, current.deadline);
                    }
                }
                self.end(
                    connection,
                    DisconnectReason::Closed,
                    Some(SocketError::domain(&error)),
                )
                .await;
            }
        }
        ReceiveOutcome::Stop
    }

    async fn end(
        &mut self,
        connection: Connection,
        reason: DisconnectReason,
        error: Option<SocketError>,
    ) {
        let student = connection.student;
        let Entry::Occupied(current) = self.current.entry(student) else {
            return;
        };
        if current.get().connection.id != connection.id
            || (matches!(reason, DisconnectReason::HeartbeatTimeout)
                && Instant::now() < current.get().deadline)
        {
            return;
        }
        let current = current.remove();
        let lost = LostConnection {
            connection,
            reason,
            observed_at: self.exam.now(),
        };
        let span = connection.span();
        self.record_lost(lost).await;
        span.in_scope(|| {
            if let Some(error) = &error {
                error.log_rejection("WebSocket message rejected");
            }
            tracing::info!(?reason, "WebSocket disconnected");
        });
        let _ = current.close.send(error);
    }

    /// Retains failed writes so the database can be reconciled before reconnect.
    async fn record_lost(&mut self, lost: LostConnection) -> bool {
        let StudentKey {
            exam_id,
            oj_user_id,
        } = lost.connection.student;
        match self
            .exam
            .disconnect_at(exam_id, oj_user_id, lost.reason, lost.observed_at)
            .await
        {
            Ok(()) | Err(DomainError::NotFound) => true,
            Err(error) => {
                if !error.is_storage_busy() {
                    lost.connection.span().in_scope(|| {
                        tracing::error!(
                            reason = %error.diagnostic(),
                            "could not record WebSocket disconnect; will retry"
                        );
                    });
                }
                self.lost.push_back(lost);
                false
            }
        }
    }

    async fn retry_lost(&mut self) {
        for _ in 0..self.lost.len().min(DISCONNECT_RETRY_BATCH) {
            let Some(lost) = self.lost.pop_front() else {
                break;
            };
            self.record_lost(lost).await;
        }
    }

    async fn dispatch(&mut self) {
        let commands = match self.exam.due_commands().await {
            Ok(commands) => commands,
            Err(error) => {
                tracing::warn!(reason = %error.diagnostic(), "command scheduling failed");
                return;
            }
        };
        for command in commands {
            let student = StudentKey {
                exam_id: command.exam_id,
                oj_user_id: command.oj_user_id,
            };
            let Some(current) = self.current.get_mut(&student) else {
                continue;
            };
            let connection = current.connection;
            if !current.send(command.command) {
                self.end(connection, DisconnectReason::Closed, None).await;
            }
        }
    }
}

impl Connection {
    pub(crate) fn span(self) -> tracing::Span {
        tracing::info_span!("websocket", route = "/api/exams/{exam_id}/ws", request_id = %self.request_id, exam_id = %self.student.exam_id, oj_user_id = self.student.oj_user_id, connection_id = %self.id)
    }
}

pub(crate) struct SocketError {
    code: &'static str,
    message: &'static str,
    reason: Cow<'static, str>,
}

impl SocketError {
    const PROTOCOL_ERROR: Self = Self {
        code: error_code::PROTOCOL_ERROR,
        message: "Invalid WebSocket message",
        reason: Cow::Borrowed("invalid protocol message"),
    };
    const INTERNAL_ERROR: Self = Self {
        code: error_code::INTERNAL_ERROR,
        message: "Internal server error",
        reason: Cow::Borrowed("internal failure"),
    };

    pub(crate) fn protocol(reason: &'static str) -> Self {
        Self {
            reason: Cow::Borrowed(reason),
            ..Self::PROTOCOL_ERROR
        }
    }

    pub(crate) fn hello_required(reason: &'static str) -> Self {
        Self {
            code: error_code::PROTOCOL_ERROR,
            message: "hello must be the first message",
            reason: Cow::Borrowed(reason),
        }
    }

    fn domain(error: &DomainError) -> Self {
        let reason = error.diagnostic();
        match error {
            DomainError::InvalidInput => Self {
                reason,
                ..Self::PROTOCOL_ERROR
            },
            DomainError::NotEnrolled => Self {
                code: error_code::NOT_ENROLLED,
                message: "Exam enrollment required",
                reason,
            },
            DomainError::Conflict => Self {
                code: error_code::COMMAND_STATE_CONFLICT,
                message: "Command result conflicts with command state",
                reason,
            },
            _ => Self {
                reason,
                ..Self::INTERNAL_ERROR
            },
        }
    }

    pub(crate) fn log_rejection(&self, message: &'static str) {
        tracing::warn!(code = self.code, reason = %self.reason, "{message}");
    }

    pub(crate) fn frame(self) -> ServerMessage {
        ServerMessage::Error {
            code: self.code.into(),
            message: self.message.into(),
        }
    }
}
