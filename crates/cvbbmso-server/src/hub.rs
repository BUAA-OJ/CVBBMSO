use std::{
    collections::{HashMap, hash_map::Entry},
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
    time::{Instant, MissedTickBehavior, interval},
};
use uuid::Uuid;

const INBOUND_CAPACITY: usize = 256;
pub(crate) const OUTBOUND_CAPACITY: usize = 32;
const SCHEDULE_INTERVAL: Duration = Duration::from_secs(1);
const DISCONNECT_RETRY_BATCH: usize = 16;

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
        processed: oneshot::Sender<()>,
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
        message: ClientMessage,
        received_at: Instant,
    ) -> bool {
        // Let each reader contribute at most one application message to the shared queue.
        let (processed, completion) = oneshot::channel();
        self.0
            .send(Request::Receive {
                connection,
                message,
                received_at,
                processed,
            })
            .await
            .is_ok()
            && completion.await.is_ok()
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
            Some(_) => {}
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
    lost: HashMap<StudentKey, LostConnection>,
    timeout: Duration,
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
                lost: HashMap::new(),
                timeout,
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
                    processed,
                } => {
                    self.receive(connection, message, received_at).await;
                    let _ = processed.send(());
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
        if !self.record_lost(connection.student).await {
            connection
                .span()
                .in_scope(|| tracing::warn!("WebSocket admission deferred while SQLite is busy"));
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
                span.in_scope(|| tracing::warn!(code = error.code, "WebSocket admission rejected"));
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
    ) {
        let Some(current) = self
            .current
            .get_mut(&connection.student)
            .filter(|current| current.connection.id == connection.id)
        else {
            return;
        };
        if received_at >= current.deadline {
            self.end(connection, DisconnectReason::HeartbeatTimeout, None)
                .await;
            return;
        }
        let heartbeat = matches!(message, ClientMessage::Heartbeat);
        match current.receive(&self.exam, message).await {
            Ok(true) => {
                if heartbeat {
                    current.deadline = received_at + self.timeout;
                }
            }
            Ok(false) => self.end(connection, DisconnectReason::Closed, None).await,
            Err(error) => {
                self.end(
                    connection,
                    DisconnectReason::Closed,
                    Some(SocketError::domain(&error)),
                )
                .await;
            }
        }
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
        self.lost.insert(
            student,
            LostConnection {
                connection,
                reason,
                observed_at: self.exam.now(),
            },
        );
        let span = connection.span();
        if !self.record_lost(student).await {
            span.in_scope(|| tracing::warn!("SQLite is busy; deferring disconnect record"));
        }
        span.in_scope(|| {
            if let Some(error) = error {
                tracing::warn!(code = error.code, "WebSocket message rejected");
            }
            tracing::info!(?reason, "WebSocket disconnected");
        });
        let _ = current.close.send(error);
    }

    /// Returns false when SQLite is busy and the record must be retried.
    async fn record_lost(&mut self, student: StudentKey) -> bool {
        let Some(lost) = self.lost.get(&student).copied() else {
            return true;
        };
        let StudentKey {
            exam_id,
            oj_user_id,
        } = student;
        match self
            .exam
            .disconnect_at(exam_id, oj_user_id, lost.reason, lost.observed_at)
            .await
        {
            Err(error) if error.is_storage_busy() => return false,
            Err(error) => lost
                .connection
                .span()
                .in_scope(|| tracing::error!(?error, "could not record WebSocket disconnect")),
            Ok(()) => {}
        }
        self.lost.remove(&student);
        true
    }

    async fn retry_lost(&mut self) {
        for _ in 0..DISCONNECT_RETRY_BATCH {
            let Some(student) = self.lost.keys().next().copied() else {
                break;
            };
            if !self.record_lost(student).await {
                // A locked database applies to all pending disconnects.
                break;
            }
        }
    }

    async fn dispatch(&mut self) {
        let commands = match self.exam.due_commands().await {
            Ok(commands) => commands,
            Err(error) => {
                tracing::warn!(%error, "command scheduling failed");
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct SocketError {
    code: &'static str,
    message: &'static str,
}

impl SocketError {
    pub(crate) const PROTOCOL_ERROR: Self = Self {
        code: error_code::PROTOCOL_ERROR,
        message: "Invalid WebSocket message",
    };
    pub(crate) const HELLO_REQUIRED: Self = Self {
        code: error_code::PROTOCOL_ERROR,
        message: "hello must be the first message",
    };
    const INTERNAL_ERROR: Self = Self {
        code: error_code::INTERNAL_ERROR,
        message: "Internal server error",
    };

    fn domain(error: &DomainError) -> Self {
        match error {
            DomainError::InvalidInput => Self::PROTOCOL_ERROR,
            DomainError::NotEnrolled => Self {
                code: error_code::NOT_ENROLLED,
                message: "Exam enrollment required",
            },
            DomainError::Conflict => Self {
                code: error_code::COMMAND_STATE_CONFLICT,
                message: "Command result conflicts with command state",
            },
            _ => Self::INTERNAL_ERROR,
        }
    }

    pub(crate) fn frame(self) -> ServerMessage {
        ServerMessage::Error {
            code: self.code.into(),
            message: self.message.into(),
        }
    }
}
