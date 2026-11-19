use std::{collections::HashMap, sync::Arc, time::Duration};

use cvbbmso_domain::{DisconnectReason, DomainError, ExamService};
use cvbbmso_protocol::{ClientHello, ClientMessage, ServerMessage, error_code};
use tokio::{
    sync::{mpsc, oneshot},
    time::{self, Instant, MissedTickBehavior},
};
use uuid::Uuid;

const INBOUND_CAPACITY: usize = 256;
pub(crate) const OUTBOUND_CAPACITY: usize = 32;
const SCHEDULE_INTERVAL: Duration = Duration::from_secs(1);

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
        self.0
            .send(Request::Receive {
                connection,
                message,
                received_at,
            })
            .await
            .is_ok()
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
        let mut ticks = time::interval(SCHEDULE_INTERVAL);
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
}

pub(crate) struct Manager {
    exam: Arc<ExamService>,
    requests: mpsc::Receiver<Request>,
    current: HashMap<StudentKey, Current>,
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
                timeout,
            },
        )
    }

    pub(crate) async fn run(mut self) -> Result<(), DomainError> {
        while let Some(request) = self.requests.recv().await {
            match request {
                Request::Connect {
                    connection,
                    hello,
                    peer,
                } => {
                    self.connect(connection, hello, peer).await?;
                }
                Request::Receive {
                    connection,
                    message,
                    received_at,
                } => {
                    self.receive(connection, message, received_at).await?;
                }
                Request::End {
                    connection,
                    reason,
                    error,
                } => {
                    self.end(connection, reason, error).await?;
                }
                Request::Schedule => self.dispatch().await?,
                #[cfg(test)]
                Request::Barrier(done) => {
                    let _ = done.send(());
                }
            }
        }
        Ok(())
    }

    async fn connect(
        &mut self,
        connection: Connection,
        hello: ClientHello,
        peer: Peer,
    ) -> Result<(), DomainError> {
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
                return Ok(());
            }
        };

        let deadline = Instant::now() + self.timeout;
        let current = Current {
            connection,
            deadline,
            frames: peer.frames,
            close: peer.close,
        };
        if let Some(previous) = self.current.insert(connection.student, current) {
            let _ = previous.close.send(None);
        }
        span.in_scope(|| tracing::info!("WebSocket admitted"));
        if !self.enqueue(
            connection.student,
            std::iter::once(outcome.sync).chain(outcome.commands),
        ) || peer.admitted.send(deadline).is_err()
        {
            self.end(connection, DisconnectReason::Closed, None).await?;
        }
        Ok(())
    }

    async fn receive(
        &mut self,
        connection: Connection,
        message: ClientMessage,
        received_at: Instant,
    ) -> Result<(), DomainError> {
        let Some(current) = self.current.get_mut(&connection.student) else {
            return Ok(());
        };
        if current.connection.id != connection.id {
            return Ok(());
        }
        if received_at >= current.deadline {
            return self
                .end(connection, DisconnectReason::HeartbeatTimeout, None)
                .await;
        }
        let heartbeat = matches!(message, ClientMessage::Heartbeat);
        let StudentKey {
            exam_id,
            oj_user_id,
        } = connection.student;
        match self.exam.receive(exam_id, oj_user_id, message).await {
            Ok(replies) => {
                if heartbeat {
                    current.deadline = received_at + self.timeout;
                }
                if !self.enqueue(connection.student, replies) {
                    self.end(connection, DisconnectReason::Closed, None).await?;
                }
                Ok(())
            }
            Err(error) => {
                self.end(
                    connection,
                    DisconnectReason::Closed,
                    Some(SocketError::domain(&error)),
                )
                .await
            }
        }
    }

    async fn end(
        &mut self,
        connection: Connection,
        reason: DisconnectReason,
        error: Option<SocketError>,
    ) -> Result<(), DomainError> {
        let Some(current) = self.current.get(&connection.student) else {
            return Ok(());
        };
        if current.connection.id != connection.id
            || (matches!(reason, DisconnectReason::HeartbeatTimeout)
                && Instant::now() < current.deadline)
        {
            return Ok(());
        }
        let StudentKey {
            exam_id,
            oj_user_id,
        } = connection.student;
        self.exam.disconnect(exam_id, oj_user_id, reason).await?;
        if let Some(current) = self.current.remove(&connection.student) {
            connection.span().in_scope(|| {
                if let Some(error) = error {
                    tracing::warn!(code = error.code, "WebSocket message rejected");
                }
                tracing::info!(?reason, "WebSocket disconnected");
            });
            let _ = current.close.send(error);
        }
        Ok(())
    }

    async fn dispatch(&mut self) -> Result<(), DomainError> {
        let commands = match self.exam.due_commands().await {
            Ok(commands) => commands,
            Err(error) => {
                tracing::warn!(%error, "command scheduling failed");
                return Ok(());
            }
        };
        for command in commands {
            let student = StudentKey {
                exam_id: command.exam_id,
                oj_user_id: command.oj_user_id,
            };
            let Some(current) = self.current.get(&student) else {
                continue;
            };
            let connection = current.connection;
            if !self.enqueue(student, [command.command]) {
                self.end(connection, DisconnectReason::Closed, None).await?;
            }
        }
        Ok(())
    }

    fn enqueue(&self, key: StudentKey, messages: impl IntoIterator<Item = ServerMessage>) -> bool {
        self.current.get(&key).is_some_and(|current| {
            messages
                .into_iter()
                .all(|message| current.frames.try_send(message).is_ok())
        })
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
            _ => Self {
                code: error_code::INTERNAL_ERROR,
                message: "Internal server error",
            },
        }
    }

    pub(crate) fn frame(self) -> ServerMessage {
        ServerMessage::Error {
            code: self.code.into(),
            message: self.message.into(),
        }
    }
}
