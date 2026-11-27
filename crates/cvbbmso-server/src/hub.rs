use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use cvbbmso_domain::{
    AdminOperation, AdminOutput, BatchOutput, BatchWrite, DisconnectReason, DomainError,
    ExamService, HeartbeatUpdate, ScheduledCommand,
};
use cvbbmso_protocol::{
    ClientHello, ClientMessage, ClientState, CommandName, CommandStatus, ExecutionState,
    ServerMessage, error_code,
};
use time::OffsetDateTime;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval, sleep_until},
};
use uuid::Uuid;

use crate::writer::{
    DatabaseWriter, DatabaseWriterHandle, TryEnqueueError, WriteBudget, WriteCompletion,
    WritePermit, WritePriority, WriterConfig, WriterRequest,
};

const INBOUND_CAPACITY: usize = 1024;
const HEARTBEAT_INBOUND_CAPACITY: usize = 4096;
pub(crate) const OUTBOUND_CAPACITY: usize = 32;
const DISCONNECT_RETRY_BATCH: usize = 16;
const WRITE_ATTEMPTS: u8 = 4;
const WRITE_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug)]
pub(crate) struct HubConfig {
    pub(crate) heartbeat_timeout: Duration,
    pub(crate) heartbeat_flush_interval: Duration,
    pub(crate) schedule_interval: Duration,
    pub(crate) max_per_conn_pending: usize,
    pub(crate) writer: WriterConfig,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            heartbeat_timeout: Duration::from_secs(25),
            heartbeat_flush_interval: Duration::from_secs(1),
            schedule_interval: Duration::from_secs(1),
            max_per_conn_pending: 4,
            writer: WriterConfig::default(),
        }
    }
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
pub(crate) struct Hub {
    requests: mpsc::Sender<Request>,
    heartbeats: mpsc::Sender<HeartbeatMsg>,
    budget: Arc<WriteBudget>,
    exam: Arc<ExamService>,
    config: HubConfig,
}

pub(crate) struct Peer {
    pub(crate) frames: mpsc::Sender<ServerMessage>,
    pub(crate) close: oneshot::Sender<Option<SocketError>>,
    pub(crate) admitted: oneshot::Sender<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionAck {
    Deadline(Instant),
    Paused,
}

struct HeartbeatMsg {
    connection: Connection,
    received_at: Instant,
    ack: oneshot::Sender<SessionAck>,
}

enum Request {
    Connect {
        connection: Connection,
        hello: ClientHello,
        username: String,
        peer: Peer,
    },
    Reliable {
        connection: Connection,
        message: ClientMessage,
        received_at: Instant,
        permit: WritePermit,
        processed: oneshot::Sender<SessionAck>,
    },
    Backpressure {
        connection: Connection,
        paused: bool,
        ack: oneshot::Sender<SessionAck>,
    },
    End {
        connection: Connection,
        reason: DisconnectReason,
        error: Option<SocketError>,
    },
    #[allow(dead_code)] // Coordinated admin entry point for upcoming admin HTTP/UDS endpoints.
    Admin {
        actor: String,
        op: AdminOperation,
        reply: oneshot::Sender<Result<AdminOutput, DomainError>>,
    },
    ScheduledCommands {
        candidates: Vec<ScheduledCommand>,
    },
    ScheduleTick,
    Shutdown(oneshot::Sender<()>),
    #[cfg(test)]
    Barrier(oneshot::Sender<()>),
}

impl Hub {
    pub(crate) fn budget(&self) -> &Arc<WriteBudget> {
        &self.budget
    }

    pub(crate) fn max_per_conn_pending(&self) -> usize {
        self.config.max_per_conn_pending.max(1)
    }

    pub(crate) async fn connect(
        &self,
        connection: Connection,
        hello: ClientHello,
        username: String,
        peer: Peer,
    ) -> bool {
        self.requests
            .send(Request::Connect {
                connection,
                hello,
                username,
                peer,
            })
            .await
            .is_ok()
    }

    pub(crate) async fn heartbeat(
        &self,
        connection: Connection,
        received_at: Instant,
    ) -> Option<SessionAck> {
        let (ack, done) = oneshot::channel();
        self.heartbeats
            .send(HeartbeatMsg {
                connection,
                received_at,
                ack,
            })
            .await
            .ok()?;
        done.await.ok()
    }

    pub(crate) async fn pause_heartbeat(&self, connection: Connection) -> Option<SessionAck> {
        let (ack, done) = oneshot::channel();
        self.requests
            .send(Request::Backpressure {
                connection,
                paused: true,
                ack,
            })
            .await
            .ok()?;
        done.await.ok()
    }

    pub(crate) async fn resume_heartbeat(&self, connection: Connection) -> Option<SessionAck> {
        let (ack, done) = oneshot::channel();
        self.requests
            .send(Request::Backpressure {
                connection,
                paused: false,
                ack,
            })
            .await
            .ok()?;
        done.await.ok()
    }

    pub(crate) async fn enqueue_reliable(
        &self,
        connection: Connection,
        message: ClientMessage,
        received_at: Instant,
        permit: WritePermit,
    ) -> Option<oneshot::Receiver<SessionAck>> {
        let (processed, completion) = oneshot::channel();
        self.requests
            .send(Request::Reliable {
                connection,
                message,
                received_at,
                permit,
                processed,
            })
            .await
            .ok()?;
        Some(completion)
    }

    #[cfg(test)]
    pub(crate) async fn receive(
        &self,
        connection: Connection,
        message: ClientMessage,
        received_at: Instant,
        _on_retry: impl FnOnce(),
    ) -> Option<Instant> {
        if matches!(message, ClientMessage::Heartbeat) {
            return match self.heartbeat(connection, received_at).await? {
                SessionAck::Deadline(deadline) => Some(deadline),
                SessionAck::Paused => None,
            };
        }
        let payload_bytes = serde_json::to_vec(&message).map_or(256, |v| v.len());
        let permit = self.budget.acquire(payload_bytes).await;
        let completion = self
            .enqueue_reliable(connection, message, received_at, permit)
            .await?;
        match completion.await.ok()? {
            SessionAck::Deadline(deadline) => Some(deadline),
            SessionAck::Paused => None,
        }
    }

    pub(crate) async fn end(
        &self,
        connection: Connection,
        reason: DisconnectReason,
        error: Option<SocketError>,
    ) {
        let _ = self
            .requests
            .send(Request::End {
                connection,
                reason,
                error,
            })
            .await;
    }

    pub(crate) async fn admin(
        &self,
        actor: String,
        op: AdminOperation,
    ) -> Result<AdminOutput, DomainError> {
        let (reply, rx) = oneshot::channel();
        self.requests
            .send(Request::Admin { actor, op, reply })
            .await
            .map_err(|_| DomainError::InvalidInput)?;
        rx.await.map_err(|_| DomainError::InvalidInput)?
    }

    pub(crate) async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.requests.send(Request::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn trigger_schedule(&self) {
        if let Ok(candidates) = self.exam.due_command_candidates().await {
            let _ = self
                .requests
                .send(Request::ScheduledCommands { candidates })
                .await;
        }
        let _ = self.requests.send(Request::ScheduleTick).await;
        self.barrier().await;
    }

    #[cfg(test)]
    pub(crate) async fn barrier(&self) {
        let (send, receive) = oneshot::channel();
        assert!(self.requests.send(Request::Barrier(send)).await.is_ok());
        receive.await.unwrap();
    }

    pub(crate) async fn schedule(self) {
        let mut ticks = interval(self.config.schedule_interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            if self.requests.is_closed() {
                break;
            }
            match self.exam.due_command_candidates().await {
                Ok(candidates) => {
                    if self
                        .requests
                        .send(Request::ScheduledCommands { candidates })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(reason = %error.diagnostic(), "command scheduling failed");
                }
            }
            if self.requests.send(Request::ScheduleTick).await.is_err() {
                break;
            }
        }
    }
}

struct Current {
    connection: Connection,
    deadline: Instant,
    backpressure_paused: bool,
    frames: mpsc::Sender<ServerMessage>,
    close: oneshot::Sender<Option<SocketError>>,
    state: ClientState,
    exam_times: Option<(OffsetDateTime, OffsetDateTime)>,
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

    fn find_command(&mut self, command_id: Uuid) -> Option<&mut Command> {
        [&mut self.start, &mut self.end]
            .into_iter()
            .filter_map(Option::as_mut)
            .find(|command| command.id == command_id)
    }
}

#[derive(Clone, Copy)]
struct HeartbeatEntry {
    connection_id: Uuid,
    last_seen_at: OffsetDateTime,
}

#[derive(Default)]
struct HeartbeatCache {
    dirty: HashMap<StudentKey, HeartbeatEntry>,
    in_flight: Option<(Uuid, HashMap<StudentKey, HeartbeatEntry>)>,
    total_received: u64,
}

impl HeartbeatCache {
    fn record(&mut self, student: StudentKey, connection_id: Uuid, now: OffsetDateTime) {
        self.total_received = self.total_received.saturating_add(1);
        self.dirty.insert(
            student,
            HeartbeatEntry {
                connection_id,
                last_seen_at: now,
            },
        );
    }

    fn take_student(&mut self, student: StudentKey, connection_id: Uuid) -> Option<OffsetDateTime> {
        if let Some(entry) = self.dirty.get(&student)
            && entry.connection_id == connection_id
        {
            return self.dirty.remove(&student).map(|e| e.last_seen_at);
        }
        self.in_flight
            .as_ref()
            .and_then(|(_, map)| map.get(&student))
            .filter(|entry| entry.connection_id == connection_id)
            .map(|entry| entry.last_seen_at)
    }

    fn clear_connection(&mut self, student: StudentKey, connection_id: Uuid) {
        if self
            .dirty
            .get(&student)
            .is_some_and(|entry| entry.connection_id == connection_id)
        {
            self.dirty.remove(&student);
        }
    }

    fn clear_student(&mut self, student: StudentKey) {
        self.dirty.remove(&student);
    }
}

struct LostConnection {
    connection: Connection,
    reason: DisconnectReason,
    observed_at: OffsetDateTime,
    last_seen_at: Option<OffsetDateTime>,
    pending_error: Option<SocketError>,
    close: Option<(oneshot::Sender<Option<SocketError>>, Option<SocketError>)>,
}

enum ReliableOp {
    Status(ClientState),
    Event {
        seq: i64,
        event: cvbbmso_protocol::ExamEvent,
    },
    CommandResult {
        command_id: Uuid,
        name: CommandName,
        status: CommandStatus,
        error_code: Option<String>,
    },
}

enum PendingOp {
    Connect {
        connection: Connection,
        hello: ClientHello,
        username: String,
        peer: Peer,
        observed_at: OffsetDateTime,
    },
    RecoverLost(LostConnection),
    Disconnect(LostConnection),
    Reliable {
        connection: Connection,
        op: ReliableOp,
        received_at: OffsetDateTime,
        attempt: u8,
        permit: WritePermit,
        processed: oneshot::Sender<SessionAck>,
    },
    AdminRemove {
        actor: String,
        op: AdminOperation,
        reply: oneshot::Sender<Result<AdminOutput, DomainError>>,
    },
}

impl PendingOp {
    fn connection_id(&self) -> Option<Uuid> {
        match self {
            Self::Connect { connection, .. } | Self::Reliable { connection, .. } => {
                Some(connection.id)
            }
            Self::Disconnect(lost) => Some(lost.connection.id),
            Self::RecoverLost(_) | Self::AdminRemove { .. } => None,
        }
    }

    fn affects_state(&self) -> bool {
        matches!(
            self,
            Self::Connect { .. }
                | Self::Reliable {
                    op: ReliableOp::Status(_) | ReliableOp::CommandResult { .. },
                    ..
                }
        )
    }

    fn reject_on_close(self) {
        if let Self::Connect { peer, .. } = self {
            let _ = peer.close.send(Some(SocketError::INTERNAL_ERROR));
        }
    }
}

enum InFlightKind {
    Connect {
        connection: Connection,
        requested_state: ClientState,
        peer: Peer,
    },
    RecoverLost(LostConnection),
    Disconnect(LostConnection),
    Reliable {
        connection: Connection,
        op: ReliableOp,
        received_at: OffsetDateTime,
        attempt: u8,
        permit: WritePermit,
        processed: oneshot::Sender<SessionAck>,
    },
    AdminRemove {
        reply: oneshot::Sender<Result<AdminOutput, DomainError>>,
    },
}

struct InFlightOp {
    operation_id: Uuid,
    student: StudentKey,
    kind: InFlightKind,
}

struct WaitingRetry {
    connection: Connection,
    op: ReliableOp,
    received_at: OffsetDateTime,
    attempt: u8,
    permit: WritePermit,
    processed: oneshot::Sender<SessionAck>,
}

struct DeferredReadCheck {
    connection: Connection,
    command_id: Uuid,
    permit: WritePermit,
    processed: oneshot::Sender<SessionAck>,
}

#[derive(Default)]
struct StudentSlot {
    current: Option<Current>,
    executing: Option<Uuid>,
    waiting_retry: Option<(Uuid, WaitingRetry)>,
    pending: VecDeque<PendingOp>,
    deferred_read_checks: Vec<DeferredReadCheck>,
    lost: Option<LostConnection>,
    barrier: bool,
    closing: bool,
    schedule_dirty: bool,
}

impl StudentSlot {
    fn is_busy(&self) -> bool {
        self.executing.is_some() || self.waiting_retry.is_some()
    }

    fn has_state_affecting_work(&self) -> bool {
        self.is_busy() || self.pending.iter().any(PendingOp::affects_state)
    }

    fn is_idle(&self) -> bool {
        self.current.is_none()
            && self.executing.is_none()
            && self.waiting_retry.is_none()
            && self.pending.is_empty()
            && self.deferred_read_checks.is_empty()
            && self.lost.is_none()
            && !self.barrier
    }
}

enum StandaloneKind {
    Admin {
        reply: oneshot::Sender<Result<AdminOutput, DomainError>>,
    },
}

struct StandaloneOp {
    kind: StandaloneKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StorageHealth {
    Healthy,
    Faulted,
    Probing,
}

pub(crate) struct Manager {
    exam: Arc<ExamService>,
    config: HubConfig,
    requests: mpsc::Receiver<Request>,
    heartbeat_rx: mpsc::Receiver<HeartbeatMsg>,
    writer: DatabaseWriterHandle,
    writer_task: Option<JoinHandle<()>>,
    completions: mpsc::UnboundedReceiver<WriteCompletion>,
    writer_backpressured: bool,
    students: HashMap<StudentKey, StudentSlot>,
    ready_students: VecDeque<StudentKey>,
    ready_set: HashSet<StudentKey>,
    in_flight_ops: HashMap<Uuid, InFlightOp>,
    standalone_ops: HashMap<Uuid, StandaloneOp>,
    standalone_queue: VecDeque<(Uuid, BatchWrite, StandaloneKind)>,
    retry_timers: BTreeMap<(Instant, Uuid), StudentKey>,
    heartbeats: HeartbeatCache,
    read_checks_tx: mpsc::UnboundedSender<ReadCheckCompletion>,
    read_checks_rx: mpsc::UnboundedReceiver<ReadCheckCompletion>,
    admin_queries_done_tx: mpsc::UnboundedSender<()>,
    admin_queries_done_rx: mpsc::UnboundedReceiver<()>,
    in_flight_admin_queries: usize,
    storage: StorageHealth,
    shutting_down: bool,
    shutdown_waiters: Vec<oneshot::Sender<()>>,
    #[cfg(test)]
    barrier_waiters: Vec<oneshot::Sender<()>>,
    #[cfg(test)]
    pub(crate) write_retries: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    pub(crate) batch_waiting: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    pub(crate) storage_recovered: Arc<tokio::sync::Notify>,
}

struct ReadCheckCompletion {
    connection: Connection,
    command_id: Uuid,
    result: Result<(), DomainError>,
    permit: WritePermit,
    processed: oneshot::Sender<SessionAck>,
}

impl Manager {
    #[cfg(test)]
    pub(crate) fn new(exam: Arc<ExamService>, timeout: Duration) -> (Hub, Self) {
        Self::with_config(
            exam,
            HubConfig {
                heartbeat_timeout: timeout,
                ..HubConfig::default()
            },
        )
    }

    pub(crate) fn with_config(exam: Arc<ExamService>, config: HubConfig) -> (Hub, Self) {
        let (request_tx, requests) = mpsc::channel(INBOUND_CAPACITY);
        let (heartbeat_tx, heartbeat_rx) = mpsc::channel(HEARTBEAT_INBOUND_CAPACITY);
        let (writer_handle, completions, writer) =
            DatabaseWriter::new(Arc::clone(&exam), config.writer);
        let budget = Arc::clone(writer_handle.budget());
        #[cfg(test)]
        let batch_waiting = Arc::clone(writer_handle.batch_waiting());
        let writer_task = tokio::spawn(writer.run());
        let (read_checks_tx, read_checks_rx) = mpsc::unbounded_channel();
        let (admin_queries_done_tx, admin_queries_done_rx) = mpsc::unbounded_channel();
        let hub = Hub {
            requests: request_tx,
            heartbeats: heartbeat_tx,
            budget,
            exam: Arc::clone(&exam),
            config,
        };
        let manager = Self {
            exam,
            config,
            requests,
            heartbeat_rx,
            writer: writer_handle,
            writer_task: Some(writer_task),
            completions,
            writer_backpressured: false,
            students: HashMap::new(),
            ready_students: VecDeque::new(),
            ready_set: HashSet::new(),
            in_flight_ops: HashMap::new(),
            standalone_ops: HashMap::new(),
            standalone_queue: VecDeque::new(),
            retry_timers: BTreeMap::new(),
            heartbeats: HeartbeatCache::default(),
            read_checks_tx,
            read_checks_rx,
            admin_queries_done_tx,
            admin_queries_done_rx,
            in_flight_admin_queries: 0,
            storage: StorageHealth::Healthy,
            shutting_down: false,
            shutdown_waiters: Vec::new(),
            #[cfg(test)]
            barrier_waiters: Vec::new(),
            #[cfg(test)]
            write_retries: Arc::default(),
            #[cfg(test)]
            batch_waiting,
            #[cfg(test)]
            storage_recovered: Arc::default(),
        };
        (hub, manager)
    }

    pub(crate) async fn run(mut self) {
        let mut heartbeat_flush = interval(self.config.heartbeat_flush_interval);
        heartbeat_flush.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let writer_ready = Arc::clone(self.writer.writer_ready());

        loop {
            self.pump_writer();
            #[cfg(test)]
            self.resolve_barriers();
            if self.shutting_down && self.is_drained() {
                self.finish_shutdown().await;
                break;
            }

            let next_retry = self.retry_timers.keys().next().map(|(at, _)| *at);
            tokio::select! {
                biased;
                Some(completion) = self.completions.recv() => {
                    self.handle_batch_completion(completion);
                }
                Some(check) = self.read_checks_rx.recv() => {
                    self.handle_read_check(check);
                }
                Some(()) = self.admin_queries_done_rx.recv() => {
                    self.in_flight_admin_queries = self.in_flight_admin_queries.saturating_sub(1);
                }
                () = writer_ready.notified(), if self.writer_backpressured => {
                    self.writer_backpressured = false;
                }
                () = sleep_until_some(next_retry) => {
                    self.fire_due_retries();
                }
                Some(msg) = self.heartbeat_rx.recv() => {
                    self.handle_heartbeat(msg);
                }
                request = self.requests.recv() => match request {
                    Some(request) => self.handle_request(request),
                    None => {
                        if !self.shutting_down {
                            self.begin_shutdown();
                        } else if self.is_drained() {
                            self.finish_shutdown().await;
                            break;
                        }
                    }
                },
                _ = heartbeat_flush.tick() => {
                    self.flush_heartbeats();
                }
            }
        }
    }

    fn handle_request(&mut self, request: Request) {
        match request {
            Request::Connect {
                connection,
                hello,
                username,
                peer,
            } => self.handle_connect(connection, hello, username, peer),
            Request::Reliable {
                connection,
                message,
                received_at,
                permit,
                processed,
            } => self.handle_reliable(connection, message, received_at, permit, processed),
            Request::Backpressure {
                connection,
                paused,
                ack,
            } => self.handle_backpressure(connection, paused, ack),
            Request::End {
                connection,
                reason,
                error,
            } => self.begin_end(connection, reason, error),
            Request::Admin { actor, op, reply } => self.handle_admin(actor, op, reply),
            Request::ScheduledCommands { candidates } => self.handle_scheduled_commands(candidates),
            Request::ScheduleTick => {
                self.retry_lost();
                self.flush_heartbeats();
                if self.storage == StorageHealth::Faulted
                    && !self.shutting_down
                    && self.writer.try_send(WriterRequest::StorageProbe).is_ok()
                {
                    self.storage = StorageHealth::Probing;
                }
            }
            Request::Shutdown(done) => {
                self.shutdown_waiters.push(done);
                self.begin_shutdown();
            }
            #[cfg(test)]
            Request::Barrier(done) => {
                self.barrier_waiters.push(done);
            }
        }
    }

    fn handle_backpressure(
        &mut self,
        connection: Connection,
        paused: bool,
        ack: oneshot::Sender<SessionAck>,
    ) {
        if self.shutting_down {
            return;
        }
        let Some(slot) = self.students.get_mut(&connection.student) else {
            return;
        };
        if slot.closing {
            return;
        }
        let Some(current) = slot.current.as_mut() else {
            return;
        };
        if current.connection.id != connection.id {
            return;
        }
        if paused {
            current.backpressure_paused = true;
            let _ = ack.send(SessionAck::Paused);
        } else {
            current.backpressure_paused = false;
            current.deadline = current
                .deadline
                .max(Instant::now() + self.config.heartbeat_timeout);
            let _ = ack.send(SessionAck::Deadline(current.deadline));
        }
    }

    fn handle_heartbeat(&mut self, msg: HeartbeatMsg) {
        if self.shutting_down {
            return;
        }
        let student = msg.connection.student;
        let Some(slot) = self.students.get_mut(&student) else {
            return;
        };
        if slot.closing {
            return;
        }
        if slot.barrier {
            let _ = msg.ack.send(SessionAck::Paused);
            return;
        }
        let Some(current) = slot.current.as_mut() else {
            return;
        };
        if current.connection.id != msg.connection.id {
            return;
        }
        if msg.received_at >= current.deadline {
            self.begin_end(msg.connection, DisconnectReason::HeartbeatTimeout, None);
            return;
        }
        current.deadline = current
            .deadline
            .max(msg.received_at + self.config.heartbeat_timeout);
        let deadline = current.deadline;
        self.heartbeats
            .record(student, msg.connection.id, self.exam.now());
        let _ = msg.ack.send(SessionAck::Deadline(deadline));
    }

    fn handle_connect(
        &mut self,
        connection: Connection,
        hello: ClientHello,
        username: String,
        peer: Peer,
    ) {
        if self.shutting_down || self.storage != StorageHealth::Healthy {
            let _ = peer.close.send(Some(SocketError::INTERNAL_ERROR));
            return;
        }
        let student = connection.student;
        let slot = self.students.entry(student).or_default();
        if let Some(lost) = slot.lost.take() {
            slot.pending.push_back(PendingOp::RecoverLost(lost));
        }
        slot.pending.push_back(PendingOp::Connect {
            connection,
            hello,
            username,
            peer,
            observed_at: self.exam.now(),
        });
        self.mark_ready(student);
    }

    fn handle_reliable(
        &mut self,
        connection: Connection,
        message: ClientMessage,
        received_at: Instant,
        permit: WritePermit,
        processed: oneshot::Sender<SessionAck>,
    ) {
        if self.shutting_down {
            return;
        }
        let student = connection.student;
        let Some(slot) = self.students.get_mut(&student) else {
            return;
        };
        if slot.closing {
            return;
        }
        let no_queued_work = !slot.is_busy() && slot.pending.is_empty();
        let Some(current) = slot.current.as_mut() else {
            return;
        };
        if current.connection.id != connection.id {
            return;
        }
        if received_at >= current.deadline {
            self.begin_end(connection, DisconnectReason::HeartbeatTimeout, None);
            return;
        }

        let op = match message {
            ClientMessage::CommandResult {
                command_id,
                status,
                error_code,
            } => {
                let Some(command) = current.find_command(command_id) else {
                    self.begin_end(
                        connection,
                        DisconnectReason::Closed,
                        Some(SocketError::domain(&DomainError::Conflict)),
                    );
                    return;
                };
                let result = (status, error_code);
                match &command.result {
                    Some(previous) if previous != &result => {
                        self.begin_end(
                            connection,
                            DisconnectReason::Closed,
                            Some(SocketError::domain(&DomainError::Conflict)),
                        );
                        return;
                    }
                    Some(_) if no_queued_work => {
                        self.spawn_read_check(connection, command_id, permit, processed);
                        return;
                    }
                    _ => ReliableOp::CommandResult {
                        command_id,
                        name: command.name,
                        status,
                        error_code: result.1,
                    },
                }
            }
            ClientMessage::Status { state } => ReliableOp::Status(state),
            ClientMessage::Event { seq, event } => ReliableOp::Event { seq, event },
            _ => {
                self.begin_end(
                    connection,
                    DisconnectReason::Closed,
                    Some(SocketError::domain(&DomainError::InvalidInput)),
                );
                return;
            }
        };

        slot.pending.push_back(PendingOp::Reliable {
            connection,
            op,
            received_at: self.exam.now(),
            attempt: 1,
            permit,
            processed,
        });
        self.mark_ready(student);
    }

    fn spawn_read_check(
        &self,
        connection: Connection,
        command_id: Uuid,
        permit: WritePermit,
        processed: oneshot::Sender<SessionAck>,
    ) {
        let exam = Arc::clone(&self.exam);
        let reply_tx = self.read_checks_tx.clone();
        tokio::spawn(async move {
            let check = exam
                .check_connected_generation(
                    connection.student.exam_id,
                    connection.student.oj_user_id,
                    Some(connection.id),
                )
                .await;
            let _ = reply_tx.send(ReadCheckCompletion {
                connection,
                command_id,
                result: check,
                permit,
                processed,
            });
        });
    }

    fn handle_read_check(&mut self, check: ReadCheckCompletion) {
        let student = check.connection.student;
        let Some(slot) = self.students.get_mut(&student) else {
            return;
        };
        if slot.closing {
            return;
        }
        if slot.barrier {
            // Defer ACK delivery until the admin removal finishes so a failed removal re-checks
            // enrollment and connection generation instead of silently dropping CommandAck.
            slot.deferred_read_checks.push(DeferredReadCheck {
                connection: check.connection,
                command_id: check.command_id,
                permit: check.permit,
                processed: check.processed,
            });
            return;
        }
        let _permit = check.permit;
        let Some(current) = slot.current.as_mut() else {
            return;
        };
        if current.connection.id != check.connection.id {
            return;
        }
        match check.result {
            Ok(()) => {
                let ack = ServerMessage::CommandAck {
                    command_id: check.command_id,
                };
                if current.frames.try_send(ack).is_err() {
                    self.begin_end(check.connection, DisconnectReason::Closed, None);
                    return;
                }
                let _ = check.processed.send(SessionAck::Deadline(current.deadline));
            }
            Err(error) => {
                self.begin_end(
                    check.connection,
                    DisconnectReason::Closed,
                    Some(SocketError::domain(&error)),
                );
            }
        }
    }

    fn begin_end(
        &mut self,
        connection: Connection,
        reason: DisconnectReason,
        error: Option<SocketError>,
    ) {
        let student = connection.student;
        let Some(slot) = self.students.get_mut(&student) else {
            return;
        };
        if slot
            .current
            .as_ref()
            .is_none_or(|current| current.connection.id != connection.id)
        {
            return;
        }
        if matches!(reason, DisconnectReason::HeartbeatTimeout)
            && slot.current.as_ref().is_some_and(|current| {
                current.backpressure_paused || Instant::now() < current.deadline
            })
        {
            return;
        }

        // Cancel any BUSY retry waiting for this connection immediately so a closed
        // connection never performs further database retries.
        let mut had_retry = false;
        if let Some((timer_id, waiting)) = slot.waiting_retry.take()
            && waiting.connection.id == connection.id
        {
            self.retry_timers.retain(|(_, id), _| *id != timer_id);
            had_retry = true;
        }

        if slot.closing {
            if error.is_some() || had_retry {
                slot.pending.retain(|op| {
                    matches!(op, PendingOp::Disconnect(_))
                        || op.connection_id() != Some(connection.id)
                });
                if let Some(error) = error {
                    for op in &mut slot.pending {
                        if let PendingOp::Disconnect(lost) = op
                            && lost.connection.id == connection.id
                        {
                            lost.pending_error = Some(error);
                            break;
                        }
                    }
                }
                self.mark_ready(student);
            }
            return;
        }

        slot.closing = true;
        let last_seen_at = self.heartbeats.take_student(student, connection.id);
        self.heartbeats.clear_connection(student, connection.id);

        // Discard unstarted ops on error, timeout, or cancelled retry; on a clean client
        // close, preserve already-received reliable ops queued ahead of Disconnect so
        // pipelined end-exam summaries and results commit in order.
        if error.is_some() || matches!(reason, DisconnectReason::HeartbeatTimeout) || had_retry {
            slot.pending
                .retain(|op| op.connection_id() != Some(connection.id));
        }

        let lost = LostConnection {
            connection,
            reason,
            observed_at: self.exam.now(),
            last_seen_at,
            pending_error: error,
            close: None,
        };
        slot.pending.push_back(PendingOp::Disconnect(lost));
        self.mark_ready(student);
    }

    fn handle_admin(
        &mut self,
        actor: String,
        op: AdminOperation,
        reply: oneshot::Sender<Result<AdminOutput, DomainError>>,
    ) {
        if let AdminOperation::ParticipantsRemove {
            exam_id,
            oj_user_id,
        } = op
        {
            let student = StudentKey {
                exam_id,
                oj_user_id,
            };
            let slot = self.students.entry(student).or_default();
            slot.barrier = true;
            slot.pending
                .push_back(PendingOp::AdminRemove { actor, op, reply });
            self.mark_ready(student);
            return;
        }

        if matches!(
            op,
            AdminOperation::ExamsCreate { .. }
                | AdminOperation::ParticipantsAdd { .. }
                | AdminOperation::ParticipantsImport { .. }
                | AdminOperation::StudentEnrollmentsImport { .. }
                | AdminOperation::StudentEnrollmentRemove { .. }
        ) {
            if self.shutting_down {
                let _ = reply.send(Err(DomainError::InvalidInput));
                return;
            }
            let op_id = Uuid::new_v4();
            self.standalone_queue.push_back((
                op_id,
                BatchWrite::Admin {
                    actor,
                    op,
                    last_seen_at: None,
                },
                StandaloneKind::Admin { reply },
            ));
            return;
        }

        // Read-only admin queries run on the read pool and overlay live Hub state.
        let overlay = self.live_student_overlay(&op);
        let admin = self.exam.admin();
        let done_tx = self.admin_queries_done_tx.clone();
        self.in_flight_admin_queries += 1;
        tokio::spawn(async move {
            let result = admin.execute(actor, op).await.map(|output| match output {
                AdminOutput::Students(mut rows) => {
                    for row in &mut rows {
                        if let Some((connected, last_seen)) = overlay.get(&row.oj_user_id) {
                            row.connected = *connected;
                            if let Some(seen) = last_seen {
                                row.last_seen_at = Some(
                                    row.last_seen_at
                                        .map_or(*seen, |existing| existing.max(*seen)),
                                );
                            }
                        }
                    }
                    AdminOutput::Students(rows)
                }
                AdminOutput::Student(mut row) => {
                    if let Some((connected, last_seen)) = overlay.get(&row.oj_user_id) {
                        row.connected = *connected;
                        if let Some(seen) = last_seen {
                            row.last_seen_at = Some(
                                row.last_seen_at
                                    .map_or(*seen, |existing| existing.max(*seen)),
                            );
                        }
                    }
                    AdminOutput::Student(row)
                }
                other => other,
            });
            let _ = reply.send(result);
            let _ = done_tx.send(());
        });
    }

    fn live_student_overlay(
        &self,
        op: &AdminOperation,
    ) -> HashMap<i64, (bool, Option<OffsetDateTime>)> {
        let latest_heartbeat = |key: &StudentKey, conn_id: Uuid| -> Option<OffsetDateTime> {
            let dirty = self
                .heartbeats
                .dirty
                .get(key)
                .filter(|entry| entry.connection_id == conn_id)
                .map(|entry| entry.last_seen_at);
            let in_flight = self
                .heartbeats
                .in_flight
                .as_ref()
                .and_then(|(_, map)| map.get(key))
                .filter(|entry| entry.connection_id == conn_id)
                .map(|entry| entry.last_seen_at);
            match (dirty, in_flight) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            }
        };
        let mut overlay = HashMap::new();
        match *op {
            AdminOperation::StudentsStatus { exam_id } => {
                for (key, slot) in &self.students {
                    if key.exam_id == exam_id
                        && let Some(current) = &slot.current
                    {
                        overlay.insert(
                            key.oj_user_id,
                            (true, latest_heartbeat(key, current.connection.id)),
                        );
                    }
                }
            }
            AdminOperation::StudentsShow {
                exam_id,
                oj_user_id,
            } => {
                let key = StudentKey {
                    exam_id,
                    oj_user_id,
                };
                if let Some(slot) = self.students.get(&key)
                    && let Some(current) = &slot.current
                {
                    overlay.insert(
                        oj_user_id,
                        (true, latest_heartbeat(&key, current.connection.id)),
                    );
                }
            }
            _ => {}
        }
        overlay
    }

    fn handle_scheduled_commands(&mut self, candidates: Vec<ScheduledCommand>) {
        if self.shutting_down {
            return;
        }
        for candidate in candidates {
            let student = StudentKey {
                exam_id: candidate.exam_id,
                oj_user_id: candidate.oj_user_id,
            };
            let Some(slot) = self.students.get_mut(&student) else {
                continue;
            };
            if slot.barrier || slot.closing {
                continue;
            }
            let has_state_work = slot.has_state_affecting_work();
            let Some(current) = slot.current.as_mut() else {
                continue;
            };
            if let Some(expected) = candidate.connection_id
                && current.connection.id != expected
            {
                continue;
            }
            current.exam_times = Some((candidate.start_at, candidate.end_at));
            if has_state_work {
                slot.schedule_dirty = true;
                continue;
            }
            let command = if current.state == candidate.state {
                candidate.command
            } else if let Some(re_eval) =
                self.exam
                    .due_command_for(current.state, candidate.start_at, candidate.end_at)
            {
                re_eval
            } else {
                continue;
            };
            let connection = current.connection;
            if !current.send(command) {
                self.begin_end(connection, DisconnectReason::Closed, None);
            }
        }
    }

    fn retry_lost(&mut self) {
        if self.shutting_down {
            return;
        }
        let mut scheduled = 0usize;
        let keys: Vec<StudentKey> = self
            .students
            .iter()
            .filter_map(|(key, slot)| {
                (slot.lost.is_some()
                    && slot.current.is_none()
                    && !slot.is_busy()
                    && slot.pending.is_empty())
                .then_some(*key)
            })
            .take(DISCONNECT_RETRY_BATCH)
            .collect();
        for key in keys {
            if let Some(slot) = self.students.get_mut(&key)
                && let Some(lost) = slot.lost.take()
            {
                slot.pending.push_back(PendingOp::RecoverLost(lost));
                self.mark_ready(key);
                scheduled += 1;
                if scheduled >= DISCONNECT_RETRY_BATCH {
                    break;
                }
            }
        }
    }

    fn flush_heartbeats(&mut self) {
        if self.heartbeats.in_flight.is_some() || self.heartbeats.dirty.is_empty() {
            return;
        }
        let dirty = std::mem::take(&mut self.heartbeats.dirty);
        let updates: Vec<HeartbeatUpdate> = dirty
            .iter()
            .map(|(student, entry)| HeartbeatUpdate {
                exam_id: student.exam_id,
                oj_user_id: student.oj_user_id,
                connection_id: entry.connection_id,
                last_seen_at: entry.last_seen_at,
            })
            .collect();
        let batch_id = Uuid::new_v4();
        match self
            .writer
            .try_send(WriterRequest::Heartbeats { batch_id, updates })
        {
            Ok(()) => {
                self.heartbeats.in_flight = Some((batch_id, dirty));
                if self.heartbeats.total_received > 0 {
                    tracing::debug!(
                        heartbeats_received = self.heartbeats.total_received,
                        flushed = self
                            .heartbeats
                            .in_flight
                            .as_ref()
                            .map_or(0, |(_, m)| m.len()),
                        "flushing coalesced heartbeat batch"
                    );
                    self.heartbeats.total_received = 0;
                }
            }
            Err(TryEnqueueError::Full(boxed)) => {
                if matches!(*boxed, WriterRequest::Heartbeats { .. }) {
                    self.heartbeats.dirty = dirty;
                    self.writer_backpressured = true;
                }
            }
            Err(TryEnqueueError::Closed) => {}
        }
    }

    fn mark_ready(&mut self, student: StudentKey) {
        if self.ready_set.insert(student) {
            self.ready_students.push_back(student);
        }
    }

    fn pump_writer(&mut self) {
        while let Some((op_id, op, kind)) = self.standalone_queue.pop_front() {
            let req = WriterRequest::Write {
                operation_id: op_id,
                priority: WritePriority::Immediate,
                op,
            };
            match self.writer.try_send(req) {
                Ok(()) => {
                    self.standalone_ops.insert(op_id, StandaloneOp { kind });
                }
                Err(TryEnqueueError::Full(boxed)) => {
                    if let WriterRequest::Write { op, .. } = *boxed {
                        self.standalone_queue.push_front((op_id, op, kind));
                    }
                    self.writer_backpressured = true;
                    return;
                }
                Err(TryEnqueueError::Closed) => return,
            }
        }

        while let Some(student) = self.ready_students.pop_front() {
            self.ready_set.remove(&student);
            let Some(slot) = self.students.get_mut(&student) else {
                continue;
            };
            if slot.is_busy() {
                continue;
            }
            let Some(next) = slot.pending.pop_front() else {
                if slot.is_idle() {
                    self.students.remove(&student);
                }
                continue;
            };

            let dispatch = self.prepare_dispatch(student, next);
            let Some((priority, batch_write, in_flight)) = dispatch else {
                if let Some(slot) = self.students.get(&student)
                    && !slot.pending.is_empty()
                {
                    self.mark_ready(student);
                } else if self
                    .students
                    .get(&student)
                    .is_some_and(StudentSlot::is_idle)
                {
                    self.students.remove(&student);
                }
                continue;
            };

            let op_id = in_flight.operation_id;
            let req = WriterRequest::Write {
                operation_id: op_id,
                priority,
                op: batch_write,
            };
            match self.writer.try_send(req) {
                Ok(()) => {
                    if let Some(slot) = self.students.get_mut(&student) {
                        slot.executing = Some(op_id);
                    }
                    self.in_flight_ops.insert(op_id, in_flight);
                }
                Err(TryEnqueueError::Full(boxed)) => {
                    // Put the operation back without losing FIFO position.
                    if let WriterRequest::Write { op, .. } = *boxed {
                        self.requeue_unstarted(student, op, in_flight.kind);
                    }
                    self.ready_set.insert(student);
                    self.ready_students.push_front(student);
                    self.writer_backpressured = true;
                    break;
                }
                Err(TryEnqueueError::Closed) => {
                    self.fail_on_writer_closed(student, in_flight.kind);
                }
            }
        }
    }

    fn prepare_dispatch(
        &mut self,
        student: StudentKey,
        op: PendingOp,
    ) -> Option<(WritePriority, BatchWrite, InFlightOp)> {
        let operation_id = Uuid::new_v4();
        match op {
            PendingOp::Connect {
                connection,
                hello,
                username,
                peer,
                observed_at,
            } => {
                let requested_state = hello.state;
                let write = BatchWrite::ConnectVerified {
                    exam_id: student.exam_id,
                    oj_user_id: student.oj_user_id,
                    username,
                    connection_id: connection.id,
                    hello,
                    observed_at,
                };
                Some((
                    WritePriority::Immediate,
                    write,
                    InFlightOp {
                        operation_id,
                        student,
                        kind: InFlightKind::Connect {
                            connection,
                            requested_state,
                            peer,
                        },
                    },
                ))
            }
            PendingOp::RecoverLost(lost) => {
                let write = BatchWrite::Disconnect {
                    exam_id: student.exam_id,
                    oj_user_id: student.oj_user_id,
                    connection_id: Some(lost.connection.id),
                    reason: lost.reason,
                    observed_at: lost.observed_at,
                    last_seen_at: lost.last_seen_at,
                };
                Some((
                    WritePriority::Immediate,
                    write,
                    InFlightOp {
                        operation_id,
                        student,
                        kind: InFlightKind::RecoverLost(lost),
                    },
                ))
            }
            PendingOp::Disconnect(lost) => {
                Some(self.prepare_disconnect_dispatch(student, operation_id, lost))
            }
            PendingOp::Reliable {
                connection,
                op,
                received_at,
                attempt,
                permit,
                processed,
            } => self.prepare_reliable_dispatch(
                student,
                operation_id,
                connection,
                op,
                received_at,
                attempt,
                permit,
                processed,
            ),
            PendingOp::AdminRemove { actor, op, reply } => {
                let last_seen_at = self
                    .students
                    .get(&student)
                    .and_then(|slot| slot.current.as_ref())
                    .and_then(|current| {
                        self.heartbeats.take_student(student, current.connection.id)
                    });
                Some((
                    WritePriority::Immediate,
                    BatchWrite::Admin {
                        actor,
                        op,
                        last_seen_at,
                    },
                    InFlightOp {
                        operation_id,
                        student,
                        kind: InFlightKind::AdminRemove { reply },
                    },
                ))
            }
        }
    }

    fn prepare_disconnect_dispatch(
        &mut self,
        student: StudentKey,
        operation_id: Uuid,
        mut lost: LostConnection,
    ) -> (WritePriority, BatchWrite, InFlightOp) {
        if let Some(slot) = self.students.get_mut(&student) {
            slot.closing = false;
            if lost.close.is_none()
                && let Some(current) = slot.current.take()
            {
                if current.connection.id == lost.connection.id {
                    lost.close = Some((current.close, lost.pending_error.take()));
                } else {
                    slot.current = Some(current);
                }
            }
        }
        let write = BatchWrite::Disconnect {
            exam_id: student.exam_id,
            oj_user_id: student.oj_user_id,
            connection_id: Some(lost.connection.id),
            reason: lost.reason,
            observed_at: lost.observed_at,
            last_seen_at: lost.last_seen_at,
        };
        (
            WritePriority::Immediate,
            write,
            InFlightOp {
                operation_id,
                student,
                kind: InFlightKind::Disconnect(lost),
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_reliable_dispatch(
        &mut self,
        student: StudentKey,
        operation_id: Uuid,
        connection: Connection,
        op: ReliableOp,
        received_at: OffsetDateTime,
        attempt: u8,
        permit: WritePermit,
        processed: oneshot::Sender<SessionAck>,
    ) -> Option<(WritePriority, BatchWrite, InFlightOp)> {
        let slot = self.students.get_mut(&student)?;
        let current = slot.current.as_mut()?;
        if current.connection.id != connection.id {
            return None;
        }
        if !current.backpressure_paused && Instant::now() >= current.deadline {
            self.begin_end(connection, DisconnectReason::HeartbeatTimeout, None);
            return None;
        }
        let write = match &op {
            ReliableOp::Status(state) => BatchWrite::Status {
                exam_id: student.exam_id,
                oj_user_id: student.oj_user_id,
                connection_id: Some(connection.id),
                state: *state,
            },
            ReliableOp::Event { seq, event } => BatchWrite::Event {
                exam_id: student.exam_id,
                oj_user_id: student.oj_user_id,
                connection_id: Some(connection.id),
                seq: *seq,
                event: event.clone(),
                observed_at: received_at,
            },
            ReliableOp::CommandResult {
                command_id,
                name,
                status,
                error_code,
            } => {
                if let Some(command) = current.find_command(*command_id)
                    && let Some(prev) = &command.result
                {
                    if prev == &(*status, error_code.clone()) {
                        let exam = Arc::clone(&self.exam);
                        let reply_tx = self.read_checks_tx.clone();
                        let cmd_id = *command_id;
                        tokio::spawn(async move {
                            let check = exam
                                .check_connected_generation(
                                    connection.student.exam_id,
                                    connection.student.oj_user_id,
                                    Some(connection.id),
                                )
                                .await;
                            let _ = reply_tx.send(ReadCheckCompletion {
                                connection,
                                command_id: cmd_id,
                                result: check,
                                permit,
                                processed,
                            });
                        });
                        return None;
                    }
                    self.begin_end(
                        connection,
                        DisconnectReason::Closed,
                        Some(SocketError::domain(&DomainError::Conflict)),
                    );
                    return None;
                }
                BatchWrite::CommandResult {
                    exam_id: student.exam_id,
                    oj_user_id: student.oj_user_id,
                    connection_id: Some(connection.id),
                    command_id: *command_id,
                    name: *name,
                    status: *status,
                    error_code: error_code.clone(),
                }
            }
        };
        Some((
            WritePriority::Normal,
            write,
            InFlightOp {
                operation_id,
                student,
                kind: InFlightKind::Reliable {
                    connection,
                    op,
                    received_at,
                    attempt,
                    permit,
                    processed,
                },
            },
        ))
    }

    fn requeue_unstarted(&mut self, student: StudentKey, batch: BatchWrite, kind: InFlightKind) {
        let Some(slot) = self.students.get_mut(&student) else {
            return;
        };
        let pending = match (batch, kind) {
            (
                BatchWrite::ConnectVerified {
                    hello,
                    username,
                    observed_at,
                    ..
                },
                InFlightKind::Connect {
                    connection, peer, ..
                },
            ) => PendingOp::Connect {
                connection,
                hello,
                username,
                peer,
                observed_at,
            },
            (_, InFlightKind::RecoverLost(lost)) => PendingOp::RecoverLost(lost),
            (_, InFlightKind::Disconnect(lost)) => PendingOp::Disconnect(lost),
            (
                _,
                InFlightKind::Reliable {
                    connection,
                    op,
                    received_at,
                    attempt,
                    permit,
                    processed,
                },
            ) => PendingOp::Reliable {
                connection,
                op,
                received_at,
                attempt,
                permit,
                processed,
            },
            (BatchWrite::Admin { actor, op, .. }, InFlightKind::AdminRemove { reply }) => {
                PendingOp::AdminRemove { actor, op, reply }
            }
            _ => return,
        };
        slot.pending.push_front(pending);
    }

    fn fail_on_writer_closed(&mut self, student: StudentKey, kind: InFlightKind) {
        match kind {
            InFlightKind::Connect { peer, .. } => {
                let _ = peer.close.send(Some(SocketError::INTERNAL_ERROR));
            }
            InFlightKind::Disconnect(mut lost) => {
                if let Some((close, err)) = lost.close.take() {
                    let _ = close.send(err);
                }
            }
            InFlightKind::AdminRemove { reply } => {
                if let Some(slot) = self.students.get_mut(&student) {
                    slot.barrier = false;
                    slot.deferred_read_checks.clear();
                }
                let _ = reply.send(Err(DomainError::InvalidInput));
            }
            InFlightKind::RecoverLost(_) | InFlightKind::Reliable { .. } => {}
        }
    }

    fn handle_batch_completion(&mut self, completion: WriteCompletion) {
        let WriteCompletion::Batch {
            ops,
            heartbeat_batches,
            probed,
            batch_error,
        } = completion;

        if let Some(error) = &batch_error {
            if error.is_storage_fault() {
                if self.storage == StorageHealth::Healthy {
                    tracing::error!(
                        reason = %error.diagnostic(),
                        "persistent SQLite storage failure detected; pausing new admissions"
                    );
                }
                self.storage = StorageHealth::Faulted;
            } else if probed && self.storage == StorageHealth::Probing {
                self.storage = StorageHealth::Faulted;
            }
        } else if self.storage != StorageHealth::Healthy {
            self.storage = StorageHealth::Healthy;
            #[cfg(test)]
            self.storage_recovered.notify_waiters();
            tracing::info!("SQLite storage recovered; resuming admissions");
        }

        if self
            .heartbeats
            .in_flight
            .as_ref()
            .is_some_and(|(active_id, _)| heartbeat_batches.contains(active_id))
            && let Some((_, mut flushed)) = self.heartbeats.in_flight.take()
            && let Some(error) = &batch_error
        {
            tracing::warn!(
                reason = %error.diagnostic(),
                count = flushed.len(),
                "heartbeat batch flush failed; retaining newer updates"
            );
            for (student, entry) in flushed.drain() {
                let still_current = self.students.get(&student).is_some_and(|slot| {
                    !slot.barrier
                        && !slot.closing
                        && slot
                            .current
                            .as_ref()
                            .is_some_and(|c| c.connection.id == entry.connection_id)
                });
                if still_current {
                    self.heartbeats.dirty.entry(student).or_insert(entry);
                }
            }
        }

        for (op_id, outcome) in ops {
            if let Some(standalone) = self.standalone_ops.remove(&op_id) {
                match standalone.kind {
                    StandaloneKind::Admin { reply } => {
                        let res = match (&batch_error, outcome) {
                            (Some(err), _) => Err(DomainError::clone_shared(err)),
                            (None, Some(Ok(BatchOutput::Admin(out)))) => Ok(out),
                            (None, Some(Err(err))) => Err(err),
                            _ => Err(DomainError::InvalidInput),
                        };
                        let _ = reply.send(res);
                    }
                }
                continue;
            }

            let Some(in_flight) = self.in_flight_ops.remove(&op_id) else {
                continue;
            };
            let student = in_flight.student;
            if let Some(slot) = self.students.get_mut(&student)
                && slot.executing == Some(op_id)
            {
                slot.executing = None;
            }
            self.complete_student_op(student, in_flight.kind, outcome, batch_error.as_ref());
            if let Some(slot) = self.students.get(&student) {
                if !slot.pending.is_empty() && !slot.is_busy() {
                    self.mark_ready(student);
                } else if slot.is_idle() {
                    self.students.remove(&student);
                }
            }
        }
    }

    fn complete_student_op(
        &mut self,
        student: StudentKey,
        kind: InFlightKind,
        outcome: Option<Result<BatchOutput, DomainError>>,
        batch_error: Option<&Arc<DomainError>>,
    ) {
        match kind {
            InFlightKind::Connect {
                connection,
                requested_state,
                peer,
            } => {
                self.complete_connect(
                    student,
                    connection,
                    requested_state,
                    peer,
                    outcome,
                    batch_error,
                );
            }
            InFlightKind::RecoverLost(lost) => {
                self.complete_recover_lost(student, lost, outcome, batch_error);
            }
            InFlightKind::Disconnect(lost) => {
                self.complete_disconnect(student, lost, batch_error);
            }
            InFlightKind::Reliable {
                connection,
                op,
                received_at,
                attempt,
                permit,
                processed,
            } => {
                self.complete_reliable(
                    student,
                    connection,
                    op,
                    received_at,
                    attempt,
                    permit,
                    processed,
                    outcome,
                    batch_error,
                );
            }
            InFlightKind::AdminRemove { reply } => {
                let mut deferred = Vec::new();
                if let Some(slot) = self.students.get_mut(&student) {
                    slot.barrier = false;
                    if batch_error.is_none() && matches!(outcome, Some(Ok(BatchOutput::Admin(_)))) {
                        self.heartbeats.clear_student(student);
                        slot.pending.clear();
                        slot.deferred_read_checks.clear();
                        if let Some(current) = slot.current.take() {
                            let _ = current.close.send(Some(SocketError::not_enrolled()));
                        }
                    } else {
                        deferred = std::mem::take(&mut slot.deferred_read_checks);
                        self.mark_ready(student);
                    }
                }
                for check in deferred {
                    self.spawn_read_check(
                        check.connection,
                        check.command_id,
                        check.permit,
                        check.processed,
                    );
                }
                let result = match (batch_error, outcome) {
                    (Some(err), _) => Err(DomainError::clone_shared(err)),
                    (None, Some(Ok(BatchOutput::Admin(out)))) => Ok(out),
                    (None, Some(Err(err))) => Err(err),
                    _ => Err(DomainError::InvalidInput),
                };
                let _ = reply.send(result);
            }
        }
    }

    fn complete_connect(
        &mut self,
        student: StudentKey,
        connection: Connection,
        requested_state: ClientState,
        peer: Peer,
        outcome: Option<Result<BatchOutput, DomainError>>,
        batch_error: Option<&Arc<DomainError>>,
    ) {
        if let Some(err) = batch_error {
            let socket_err = SocketError::domain(err);
            connection.span().in_scope(|| {
                socket_err.log_rejection("WebSocket admission rejected");
            });
            let _ = peer.close.send(Some(socket_err));
            return;
        }
        let outcome = match outcome {
            Some(Ok(BatchOutput::Connected(outcome))) => outcome,
            Some(Err(err)) => {
                let socket_err = SocketError::domain(&err);
                connection.span().in_scope(|| {
                    socket_err.log_rejection("WebSocket admission rejected");
                });
                let _ = peer.close.send(Some(socket_err));
                return;
            }
            _ => {
                let _ = peer.close.send(Some(SocketError::INTERNAL_ERROR));
                return;
            }
        };
        if self.shutting_down {
            let _ = peer.close.send(None);
            let Some(slot) = self.students.get_mut(&student) else {
                return;
            };
            slot.pending
                .push_back(PendingOp::Disconnect(LostConnection {
                    connection,
                    reason: DisconnectReason::Closed,
                    observed_at: self.exam.now(),
                    last_seen_at: None,
                    pending_error: None,
                    close: None,
                }));
            self.mark_ready(student);
            return;
        }

        let exam_times = match &outcome.sync {
            ServerMessage::Sync {
                start_at, end_at, ..
            } => Some((*start_at, *end_at)),
            _ => None,
        };
        let deadline = Instant::now() + self.config.heartbeat_timeout;
        let mut current = Current {
            connection,
            deadline,
            backpressure_paused: false,
            frames: peer.frames,
            close: peer.close,
            state: requested_state,
            exam_times,
            start: None,
            end: None,
        };
        let delivered = std::iter::once(outcome.sync)
            .chain(outcome.commands)
            .all(|frame| current.send(frame));

        let slot = self.students.entry(student).or_default();
        slot.closing = false;
        if let Some(previous) = slot.current.replace(current) {
            self.heartbeats
                .clear_connection(student, previous.connection.id);
            slot.pending
                .retain(|op| op.connection_id() != Some(previous.connection.id));
            let _ = previous.close.send(None);
        }
        connection
            .span()
            .in_scope(|| tracing::info!("WebSocket admitted"));
        if !delivered || peer.admitted.send(deadline).is_err() {
            self.begin_end(connection, DisconnectReason::Closed, None);
        }
    }

    fn complete_recover_lost(
        &mut self,
        student: StudentKey,
        lost: LostConnection,
        outcome: Option<Result<BatchOutput, DomainError>>,
        batch_error: Option<&Arc<DomainError>>,
    ) {
        let err = match (batch_error, outcome) {
            (Some(err), _) => Some(err.diagnostic()),
            (None, Some(Err(DomainError::NotFound) | Ok(_))) => None,
            (None, Some(Err(err))) => Some(err.diagnostic()),
            (None, None) => Some(Cow::Borrowed("missing batch outcome")),
        };
        if let Some(reason) = err {
            let is_busy = batch_error.is_some_and(|e| e.is_storage_busy());
            if !is_busy {
                lost.connection.span().in_scope(|| {
                    tracing::error!(
                        %reason,
                        "could not record WebSocket disconnect; will retry"
                    );
                });
            }
            if let Some(slot) = self.students.get_mut(&student) {
                slot.lost = Some(lost);
                // Reject any pending Connect queued behind this failed lost recovery.
                while matches!(slot.pending.front(), Some(PendingOp::Connect { .. })) {
                    if let Some(op) = slot.pending.pop_front() {
                        op.reject_on_close();
                    }
                }
            }
        }
    }

    fn complete_disconnect(
        &mut self,
        student: StudentKey,
        mut lost: LostConnection,
        batch_error: Option<&Arc<DomainError>>,
    ) {
        let close = lost.close.take();
        let span = lost.connection.span();
        if let Some(err) = batch_error {
            if !err.is_storage_busy() {
                span.in_scope(|| {
                    tracing::error!(
                        reason = %err.diagnostic(),
                        "could not record WebSocket disconnect; will retry"
                    );
                });
            }
            if let Some(slot) = self.students.get_mut(&student) {
                slot.closing = false;
                slot.lost = Some(lost);
                while matches!(slot.pending.front(), Some(PendingOp::Connect { .. })) {
                    if let Some(op) = slot.pending.pop_front() {
                        op.reject_on_close();
                    }
                }
            }
        } else {
            if let Some(slot) = self.students.get_mut(&student) {
                slot.closing = false;
            }
            let reason = lost.reason;
            span.in_scope(|| {
                if let Some((_, Some(err))) = &close {
                    err.log_rejection("WebSocket message rejected");
                }
                tracing::info!(?reason, "WebSocket disconnected");
            });
        }
        if let Some((close_tx, error)) = close {
            let _ = close_tx.send(error);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn complete_reliable(
        &mut self,
        student: StudentKey,
        connection: Connection,
        op: ReliableOp,
        received_at: OffsetDateTime,
        attempt: u8,
        permit: WritePermit,
        processed: oneshot::Sender<SessionAck>,
        outcome: Option<Result<BatchOutput, DomainError>>,
        batch_error: Option<&Arc<DomainError>>,
    ) {
        let Some(slot) = self.students.get_mut(&student) else {
            return;
        };
        let has_state_work = slot.has_state_affecting_work();
        let Some(current) = slot.current.as_mut() else {
            // Clean close may have moved current into Disconnect while this op was executing.
            return;
        };
        if current.connection.id != connection.id {
            return;
        }

        if let Some(err) = batch_error {
            self.handle_reliable_error(
                student,
                connection,
                op,
                received_at,
                attempt,
                permit,
                processed,
                err,
            );
            return;
        }

        match outcome {
            Some(Ok(BatchOutput::Ack)) => {
                if let Some(state_changed) = apply_reliable_ack(current, op) {
                    let _ = processed.send(SessionAck::Deadline(current.deadline));
                    if state_changed
                        && slot.schedule_dirty
                        && !has_state_work
                        && let Some((start_at, end_at)) = current.exam_times
                    {
                        slot.schedule_dirty = false;
                        if let Some(command) =
                            self.exam.due_command_for(current.state, start_at, end_at)
                            && !current.send(command)
                        {
                            self.begin_end(connection, DisconnectReason::Closed, None);
                        }
                    }
                } else {
                    self.begin_end(connection, DisconnectReason::Closed, None);
                }
            }
            Some(Err(err)) => {
                self.handle_reliable_error(
                    student,
                    connection,
                    op,
                    received_at,
                    attempt,
                    permit,
                    processed,
                    &err,
                );
            }
            _ => {
                self.begin_end(
                    connection,
                    DisconnectReason::Closed,
                    Some(SocketError::INTERNAL_ERROR),
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_reliable_error(
        &mut self,
        student: StudentKey,
        connection: Connection,
        op: ReliableOp,
        received_at: OffsetDateTime,
        attempt: u8,
        permit: WritePermit,
        processed: oneshot::Sender<SessionAck>,
        error: &DomainError,
    ) {
        let Some(slot) = self.students.get_mut(&student) else {
            return;
        };
        let Some(current) = slot.current.as_ref() else {
            return;
        };
        if error.is_storage_busy() && !slot.closing && !self.shutting_down {
            let now = Instant::now();
            if !current.backpressure_paused && now >= current.deadline {
                self.begin_end(connection, DisconnectReason::HeartbeatTimeout, None);
                return;
            }
            if attempt < WRITE_ATTEMPTS {
                #[cfg(test)]
                self.write_retries.notify_one();
                let retry_at = if current.backpressure_paused {
                    now + WRITE_RETRY_DELAY
                } else {
                    (now + WRITE_RETRY_DELAY).min(current.deadline)
                };
                let timer_id = Uuid::new_v4();
                slot.waiting_retry = Some((
                    timer_id,
                    WaitingRetry {
                        connection,
                        op,
                        received_at,
                        attempt: attempt + 1,
                        permit,
                        processed,
                    },
                ));
                self.retry_timers.insert((retry_at, timer_id), student);
                return;
            }
        }
        let close_error =
            (!slot.closing || !error.is_storage_busy()).then(|| SocketError::domain(error));
        self.begin_end(connection, DisconnectReason::Closed, close_error);
    }

    fn fire_due_retries(&mut self) {
        let now = Instant::now();
        while let Some((&(retry_at, timer_id), _)) = self.retry_timers.first_key_value() {
            if retry_at > now {
                break;
            }
            let Some(student) = self.retry_timers.remove(&(retry_at, timer_id)) else {
                continue;
            };
            let Some(slot) = self.students.get_mut(&student) else {
                continue;
            };
            let Some((waiting_id, waiting)) = slot.waiting_retry.take() else {
                continue;
            };
            if waiting_id != timer_id {
                slot.waiting_retry = Some((waiting_id, waiting));
                continue;
            }
            let Some(current) = slot.current.as_ref() else {
                continue;
            };
            if current.connection.id != waiting.connection.id {
                continue;
            }
            if !current.backpressure_paused && now >= current.deadline {
                self.begin_end(waiting.connection, DisconnectReason::HeartbeatTimeout, None);
                continue;
            }
            slot.pending.push_front(PendingOp::Reliable {
                connection: waiting.connection,
                op: waiting.op,
                received_at: waiting.received_at,
                attempt: waiting.attempt,
                permit: waiting.permit,
                processed: waiting.processed,
            });
            self.mark_ready(student);
        }
    }

    fn begin_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        // Cancel retry timers and drop queued reliable operations that cannot commit during shutdown.
        let timer_keys: Vec<_> = self.retry_timers.keys().copied().collect();
        for key in timer_keys {
            self.retry_timers.remove(&key);
        }
        let student_keys: Vec<_> = self.students.keys().copied().collect();
        for student in student_keys {
            if let Some(slot) = self.students.get_mut(&student) {
                slot.waiting_retry = None;
                slot.pending.retain(|op| {
                    matches!(op, PendingOp::Disconnect(_) | PendingOp::RecoverLost(_))
                });
                if let Some(current) = slot.current.take() {
                    let last_seen_at = self.heartbeats.take_student(student, current.connection.id);
                    let lost = LostConnection {
                        connection: current.connection,
                        reason: DisconnectReason::Closed,
                        observed_at: self.exam.now(),
                        last_seen_at,
                        pending_error: None,
                        close: Some((current.close, None)),
                    };
                    slot.pending.push_back(PendingOp::Disconnect(lost));
                    self.mark_ready(student);
                }
            }
        }
        self.flush_heartbeats();
    }

    fn is_drained(&self) -> bool {
        self.in_flight_ops.is_empty()
            && self.standalone_ops.is_empty()
            && self.standalone_queue.is_empty()
            && self.ready_students.is_empty()
            && self.heartbeats.in_flight.is_none()
            && self.heartbeats.dirty.is_empty()
            && self.in_flight_admin_queries == 0
    }

    async fn finish_shutdown(&mut self) {
        self.writer.shutdown().await;
        if let Some(handle) = self.writer_task.take() {
            let _ = handle.await;
        }
        for waiter in self.shutdown_waiters.drain(..) {
            let _ = waiter.send(());
        }
        #[cfg(test)]
        for waiter in self.barrier_waiters.drain(..) {
            let _ = waiter.send(());
        }
    }

    #[cfg(test)]
    fn resolve_barriers(&mut self) {
        let no_executing = self.in_flight_ops.is_empty()
            && self.standalone_ops.is_empty()
            && self.standalone_queue.is_empty()
            && self.ready_students.is_empty()
            && self.heartbeats.in_flight.is_none()
            && self.in_flight_admin_queries == 0;
        if no_executing {
            for waiter in self.barrier_waiters.drain(..) {
                let _ = waiter.send(());
            }
        }
    }
}

fn apply_reliable_ack(current: &mut Current, op: ReliableOp) -> Option<bool> {
    let mut state_changed = false;
    match op {
        ReliableOp::Status(new_state) => {
            if current.state.execution != ExecutionState::Finished
                && new_state.execution != ExecutionState::Finished
            {
                current.state = new_state;
                state_changed = true;
            }
        }
        ReliableOp::CommandResult {
            command_id,
            name,
            status,
            error_code,
        } => {
            if let Some(command) = current.find_command(command_id) {
                command.result = Some((status, error_code));
            }
            let complete = status == CommandStatus::Completed;
            match name {
                CommandName::StartExam if current.state.execution == ExecutionState::Ready => {
                    current.state = ClientState {
                        execution: ExecutionState::Recording,
                        recording: if complete {
                            cvbbmso_protocol::RecordingState::Capturing
                        } else {
                            cvbbmso_protocol::RecordingState::Failed
                        },
                    };
                    state_changed = true;
                }
                CommandName::EndExam => {
                    current.state = ClientState {
                        execution: ExecutionState::Finished,
                        recording: if complete {
                            cvbbmso_protocol::RecordingState::Complete
                        } else {
                            cvbbmso_protocol::RecordingState::Failed
                        },
                    };
                    state_changed = true;
                }
                _ => {}
            }
            current
                .frames
                .try_send(ServerMessage::CommandAck { command_id })
                .ok()?;
        }
        ReliableOp::Event { .. } => {}
    }
    Some(state_changed)
}

async fn sleep_until_some(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

impl Connection {
    pub(crate) fn span(self) -> tracing::Span {
        tracing::info_span!(
            "websocket",
            route = "/api/exams/{exam_id}/ws",
            request_id = %self.request_id,
            exam_id = %self.student.exam_id,
            oj_user_id = self.student.oj_user_id,
            connection_id = %self.id
        )
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
    const NOT_ENROLLED: Self = Self {
        code: error_code::NOT_ENROLLED,
        message: "Exam enrollment required",
        reason: Cow::Borrowed("user is not enrolled"),
    };

    pub(crate) fn not_enrolled() -> Self {
        Self::NOT_ENROLLED
    }

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
                reason,
                ..Self::NOT_ENROLLED
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

#[cfg(test)]
mod tests;
