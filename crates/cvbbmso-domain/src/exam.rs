use cvbbmso_protocol::{
    ClientHello, ClientMessage, ClientState, CommandName, CommandStatus, ExamEvent, ExamSummary,
    ExecutionState, RecordingState, ServerMessage,
};
use sea_orm::{
    ConnectionTrait, DatabaseTransaction, DbBackend, EntityTrait, FromQueryResult, Set, Statement,
    TransactionTrait, Value,
    sea_query::{Expr, ExprTrait, JoinType, OnConflict, Order, Query},
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AdminOperation, AdminOutput, AdminService, DomainError, Service, admin, commands,
    db::{self, Runtime, StudentKey},
    entity::{enrollment, exam as exam_table, runtime},
    events,
};

/// A committed connection snapshot and the commands due on that connection.
#[derive(Debug)]
pub struct ConnectOutcome {
    /// The authoritative `ServerMessage::Sync` snapshot.
    pub sync: ServerMessage,
    /// Commands to send after the snapshot.
    pub commands: Vec<ServerMessage>,
}

/// A due command and the student to whom it would be delivered.
#[derive(Debug)]
pub struct DirectedCommand {
    /// The exam that issued the command.
    pub exam_id: Uuid,
    /// The enrolled Accoding user.
    pub oj_user_id: i64,
    /// The `ServerMessage::Command` to send.
    pub command: ServerMessage,
}

/// A due command candidate paired with the runtime snapshot observed when scheduling.
#[derive(Debug)]
pub struct ScheduledCommand {
    /// The exam that issued the command.
    pub exam_id: Uuid,
    /// The enrolled Accoding user.
    pub oj_user_id: i64,
    /// The connection generation recorded on the student runtime, if set.
    pub connection_id: Option<Uuid>,
    /// The runtime state observed when evaluating the schedule.
    pub state: ClientState,
    /// The exam's scheduled start time.
    pub start_at: OffsetDateTime,
    /// The exam's scheduled end time.
    pub end_at: OffsetDateTime,
    /// The `ServerMessage::Command` candidate to deliver if still valid.
    pub command: ServerMessage,
}

/// A single write operation executed inside a shared database batch transaction.
#[derive(Debug)]
#[non_exhaustive]
pub enum BatchWrite {
    /// Admits or replaces a student's connection.
    Connect {
        /// Exam identifier.
        exam_id: Uuid,
        /// Enrolled Accoding user ID.
        oj_user_id: i64,
        /// Server-assigned connection generation UUID.
        connection_id: Uuid,
        /// Client handshake payload.
        hello: ClientHello,
        /// Server UTC timestamp when `hello` was accepted.
        observed_at: OffsetDateTime,
    },
    /// Marks a student offline and records loss if unfinished.
    Disconnect {
        /// Exam identifier.
        exam_id: Uuid,
        /// Enrolled Accoding user ID.
        oj_user_id: i64,
        /// Expected active connection generation, if scoped to a specific socket.
        connection_id: Option<Uuid>,
        /// Disconnect reason.
        reason: DisconnectReason,
        /// Server UTC timestamp when the disconnect was observed.
        observed_at: OffsetDateTime,
        /// Latest valid in-memory heartbeat timestamp for this connection, if any.
        last_seen_at: Option<OffsetDateTime>,
    },
    /// Updates a connected student's state snapshot.
    Status {
        /// Exam identifier.
        exam_id: Uuid,
        /// Enrolled Accoding user ID.
        oj_user_id: i64,
        /// Expected active connection generation.
        connection_id: Option<Uuid>,
        /// Reported client state.
        state: ClientState,
    },
    /// Persists a reliable client event and advances `ack_event_through`.
    Event {
        /// Exam identifier.
        exam_id: Uuid,
        /// Enrolled Accoding user ID.
        oj_user_id: i64,
        /// Expected active connection generation.
        connection_id: Option<Uuid>,
        /// Client event sequence number.
        seq: i64,
        /// Event payload.
        event: ExamEvent,
        /// Server UTC timestamp when the event was received.
        observed_at: OffsetDateTime,
    },
    /// Archives a terminal command result and updates student state.
    CommandResult {
        /// Exam identifier.
        exam_id: Uuid,
        /// Enrolled Accoding user ID.
        oj_user_id: i64,
        /// Expected active connection generation.
        connection_id: Option<Uuid>,
        /// Issued command ID.
        command_id: Uuid,
        /// Issued command name.
        name: CommandName,
        /// Reported terminal status.
        status: CommandStatus,
        /// Optional protocol error code on failure.
        error_code: Option<String>,
    },
    /// Executes an administrative mutation and records its audit entry.
    Admin {
        /// Audit actor name.
        actor: String,
        /// Administrative operation.
        op: AdminOperation,
        /// Latest in-memory heartbeat if removing a connected participant.
        last_seen_at: Option<OffsetDateTime>,
    },
}

/// A coalesced application heartbeat timestamp to flush for an active connection.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatUpdate {
    /// Exam identifier.
    pub exam_id: Uuid,
    /// Enrolled Accoding user ID.
    pub oj_user_id: i64,
    /// Active connection generation UUID.
    pub connection_id: Uuid,
    /// Latest valid application heartbeat UTC timestamp.
    pub last_seen_at: OffsetDateTime,
}

/// Output produced by an individual [`BatchWrite`] on success.
#[derive(Debug)]
#[non_exhaustive]
pub enum BatchOutput {
    /// Output for [`BatchWrite::Connect`].
    Connected(ConnectOutcome),
    /// Output for [`BatchWrite::Admin`].
    Admin(AdminOutput),
    /// Output for operations that return unit on success.
    Ack,
}

/// How the server observed a connection's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// The transport closed.
    Closed,
    /// No application heartbeat arrived by the deadline.
    HeartbeatTimeout,
    /// A newer connection took over the same exam and student.
    Replaced,
    /// A previous server left the connection marked online.
    ServerRestart,
}

impl DisconnectReason {
    fn detail(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::HeartbeatTimeout => "heartbeat_timeout",
            Self::Replaced => "replaced",
            Self::ServerRestart => "server_restart",
        }
    }
}

/// The exam service for a single server instance.
#[derive(Clone)]
pub struct ExamService(Service);

impl ExamService {
    /// Opens a `SQLite` database and applies its migrations.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-SQLite URL or failed database initialization.
    pub async fn open(database_url: &str) -> Result<Self, DomainError> {
        Self::open_with_clock(database_url, OffsetDateTime::now_utc).await
    }

    /// Opens a `SQLite` database using a supplied clock for domain operations.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-SQLite URL or failed database initialization.
    pub async fn open_with_clock(
        database_url: &str,
        clock: impl Fn() -> OffsetDateTime + Send + Sync + 'static,
    ) -> Result<Self, DomainError> {
        Ok(Self(Service::open(database_url, clock).await?))
    }

    /// Returns an administration service sharing the database and clock.
    #[must_use]
    pub fn admin(&self) -> AdminService {
        AdminService(self.0.clone())
    }

    /// Returns the service's current UTC time for observing connection loss.
    #[must_use]
    pub fn now(&self) -> OffsetDateTime {
        self.0.now()
    }

    /// Closes the underlying write and read connection pools cleanly.
    ///
    /// # Errors
    ///
    /// Returns a storage error if closing either pool fails.
    pub async fn close(&self) -> Result<(), DomainError> {
        self.0.reader.clone().close().await?;
        self.0.writer.clone().close().await?;
        Ok(())
    }

    /// Transitions connections left online by a previous server to offline.
    ///
    /// Call once before accepting connections after server startup.
    ///
    /// # Errors
    ///
    /// Returns a storage error if recovery cannot be committed.
    pub async fn recover_online(&self) -> Result<(), DomainError> {
        let now = self.0.now();
        db::write(&self.0.writer, |tx| {
            Box::pin(async move {
                for runtime in Runtime::online(tx).await? {
                    disconnect(tx, &runtime, DisconnectReason::ServerRestart, now).await?;
                }
                Ok(())
            })
        })
        .await
    }

    /// Lists all exams in which the user is currently enrolled.
    ///
    /// # Errors
    ///
    /// Returns an error for a nonpositive user ID or failed database read.
    pub async fn list_for(&self, oj_user_id: i64) -> Result<Vec<ExamSummary>, DomainError> {
        db::validate_user(oj_user_id)?;
        db::exams(&self.0.reader, Some(oj_user_id), self.0.now()).await
    }

    /// Checks enrollment and returns the scheduled exam duration.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid user ID or failed read.
    /// A missing exam yields `DomainError::NotFound` before enrollment is checked,
    /// and a missing enrollment yields `DomainError::NotEnrolled`.
    pub async fn check_enrollment(
        &self,
        exam_id: Uuid,
        oj_user_id: i64,
    ) -> Result<time::Duration, DomainError> {
        let key = StudentKey::new(exam_id, oj_user_id)?;
        db::transaction(&self.0.reader, |tx| {
            Box::pin(async move {
                let (start, end) = db::enrollment(tx, &key).await?.times()?;
                Ok(end - start)
            })
        })
        .await
    }

    /// Checks that the student's existing connection can still receive acknowledgements.
    ///
    /// # Errors
    ///
    /// Returns an error if the student is no longer enrolled or connected, or storage fails.
    pub async fn check_connected(&self, exam_id: Uuid, oj_user_id: i64) -> Result<(), DomainError> {
        self.check_connected_generation(exam_id, oj_user_id, None)
            .await
    }

    /// Checks on the read pool that the student's connection generation is enrolled and online.
    ///
    /// # Errors
    ///
    /// Returns an error if enrollment was revoked, the connection was replaced or disconnected,
    /// or storage fails.
    pub async fn check_connected_generation(
        &self,
        exam_id: Uuid,
        oj_user_id: i64,
        connection_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        let key = StudentKey::new(exam_id, oj_user_id)?;
        db::transaction(&self.0.reader, |tx| {
            Box::pin(async move {
                Runtime::connected_generation(tx, &key, connection_id)
                    .await
                    .map(|_| ())
            })
        })
        .await
    }

    /// Establishes or replaces a student's current connection.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid hello or user ID, missing exam or enrollment,
    /// or failed database transaction.
    pub async fn connect(
        &self,
        exam_id: Uuid,
        oj_user_id: i64,
        hello: ClientHello,
    ) -> Result<ConnectOutcome, DomainError> {
        let key = StudentKey::new(exam_id, oj_user_id)?;
        validate_state(hello.state)?;
        let now = self.0.now();
        let connection_id = Uuid::new_v4();
        db::write(&self.0.writer, |tx| {
            let key = key.clone();
            let hello = hello.clone();
            Box::pin(async move { connect(tx, &key, Some(connection_id), &hello, now).await })
        })
        .await
    }

    /// Commits a heartbeat, state update, or event from the current connection.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, a missing runtime, or failed storage.
    pub async fn receive(
        &self,
        exam_id: Uuid,
        oj_user_id: i64,
        message: ClientMessage,
    ) -> Result<(), DomainError> {
        let key = StudentKey::new(exam_id, oj_user_id)?;
        let now = self.0.now();
        db::immediate_transaction(&self.0.writer, |tx| {
            Box::pin(async move { receive(tx, &key, None, message, now).await })
        })
        .await
    }

    /// Archives a terminal command result from the current connection.
    ///
    /// The caller must resolve the command ID in that connection's session
    /// and handle duplicate acknowledgements there. A successful end requires
    /// a recording summary within the student's contiguous reliable event history.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid student ID, a command result conflict,
    /// or failed storage. Archive and state changes commit together.
    pub async fn record_command_result(
        &self,
        exam_id: Uuid,
        oj_user_id: i64,
        command_id: Uuid,
        name: CommandName,
        status: CommandStatus,
        error_code: Option<String>,
    ) -> Result<(), DomainError> {
        let key = StudentKey::new(exam_id, oj_user_id)?;
        db::immediate_transaction(&self.0.writer, |tx| {
            Box::pin(commands::record(
                tx, key, None, command_id, name, status, error_code,
            ))
        })
        .await
    }

    /// Turns the current connection offline and records loss if unfinished.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid user ID, a missing runtime, or a failed
    /// database transaction.
    pub async fn disconnect(
        &self,
        exam_id: Uuid,
        oj_user_id: i64,
        reason: DisconnectReason,
    ) -> Result<(), DomainError> {
        self.disconnect_at(exam_id, oj_user_id, reason, self.now())
            .await
    }

    /// Turns the current connection offline using the original observation time.
    ///
    /// Use the same time when retrying the same loss after temporary contention.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid user ID, a missing runtime, or failed storage.
    pub async fn disconnect_at(
        &self,
        exam_id: Uuid,
        oj_user_id: i64,
        reason: DisconnectReason,
        observed_at: OffsetDateTime,
    ) -> Result<(), DomainError> {
        let key = StudentKey::new(exam_id, oj_user_id)?;
        // The connection manager retries contention between requests.
        db::immediate_transaction(&self.0.writer, |tx| {
            Box::pin(async move {
                let runtime = Runtime::get(tx, &key).await?;
                disconnect(tx, &runtime, reason, observed_at).await
            })
        })
        .await
    }

    /// Generates candidates for the commands now due for connected students.
    ///
    /// The caller sends each command at most once per session.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the schedule cannot be applied.
    pub async fn due_commands(&self) -> Result<Vec<DirectedCommand>, DomainError> {
        Ok(self
            .due_command_candidates()
            .await?
            .into_iter()
            .map(|candidate| DirectedCommand {
                exam_id: candidate.exam_id,
                oj_user_id: candidate.oj_user_id,
                command: candidate.command,
            })
            .collect())
    }

    /// Queries all currently due commands with snapshot metadata in a single set-based join.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the read-pool query fails.
    pub async fn due_command_candidates(&self) -> Result<Vec<ScheduledCommand>, DomainError> {
        let now = self.0.now();
        due_commands(&self.0.reader, now).await
    }

    /// Computes whether a command is currently due for the given state and exam bounds.
    #[must_use]
    pub fn due_command_for(
        &self,
        state: ClientState,
        start_at: OffsetDateTime,
        end_at: OffsetDateTime,
    ) -> Option<ServerMessage> {
        commands::due(state, start_at, end_at, self.0.now())
    }

    /// Probes whether the write database pool can start and commit an immediate write transaction.
    ///
    /// # Errors
    ///
    /// Returns a storage error if acquiring a write lock or committing the probe transaction fails.
    pub async fn probe_storage(&self) -> Result<(), DomainError> {
        db::immediate_transaction(&self.0.writer, |_| Box::pin(async { Ok(()) })).await
    }

    /// Executes a batch of writes and heartbeat updates in a single `BEGIN IMMEDIATE` transaction.
    ///
    /// Individual business validation errors roll back only their own savepoint and are returned
    /// per item in the output vector. Storage-level failures roll back the entire outer transaction
    /// and return `Err`.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the outer transaction fails to start, encounters a database-level
    /// error, or fails to commit.
    pub async fn execute_batch(
        &self,
        writes: Vec<BatchWrite>,
        heartbeats: Vec<HeartbeatUpdate>,
    ) -> Result<Vec<Result<BatchOutput, DomainError>>, DomainError> {
        if writes.is_empty() && heartbeats.is_empty() {
            return Ok(Vec::new());
        }
        let service = self.0.clone();
        db::immediate_transaction(&self.0.writer, move |tx| {
            Box::pin(async move {
                let mut outcomes = Vec::with_capacity(writes.len());
                for write in writes {
                    let sp = tx.begin().await?;
                    match apply_batch_write(&sp, &service, write).await {
                        Ok(output) => {
                            sp.commit().await?;
                            outcomes.push(Ok(output));
                        }
                        Err(error) if !matches!(error, DomainError::Storage { .. }) => {
                            sp.rollback().await?;
                            outcomes.push(Err(error));
                        }
                        Err(error) => {
                            let _ = sp.rollback().await;
                            return Err(error);
                        }
                    }
                }
                if !heartbeats.is_empty() {
                    flush_heartbeats(tx, &heartbeats).await?;
                }
                Ok(outcomes)
            })
        })
        .await
    }
}

async fn apply_batch_write(
    tx: &DatabaseTransaction,
    service: &Service,
    write: BatchWrite,
) -> Result<BatchOutput, DomainError> {
    match write {
        BatchWrite::Connect {
            exam_id,
            oj_user_id,
            connection_id,
            hello,
            observed_at,
        } => {
            let key = StudentKey::new(exam_id, oj_user_id)?;
            validate_state(hello.state)?;
            connect(tx, &key, Some(connection_id), &hello, observed_at)
                .await
                .map(BatchOutput::Connected)
        }
        BatchWrite::Disconnect {
            exam_id,
            oj_user_id,
            connection_id,
            reason,
            observed_at,
            last_seen_at,
        } => {
            let key = StudentKey::new(exam_id, oj_user_id)?;
            let runtime = Runtime::get(tx, &key).await?;
            if let Some(expected) = connection_id
                && runtime.connection_id.as_deref() != Some(&expected.to_string())
            {
                return Ok(BatchOutput::Ack);
            }
            disconnect_with_last_seen(tx, &runtime, reason, observed_at, last_seen_at).await?;
            Ok(BatchOutput::Ack)
        }
        BatchWrite::Status {
            exam_id,
            oj_user_id,
            connection_id,
            state,
        } => {
            let key = StudentKey::new(exam_id, oj_user_id)?;
            receive(
                tx,
                &key,
                connection_id,
                ClientMessage::Status { state },
                service.now(),
            )
            .await?;
            Ok(BatchOutput::Ack)
        }
        BatchWrite::Event {
            exam_id,
            oj_user_id,
            connection_id,
            seq,
            event,
            observed_at,
        } => {
            let key = StudentKey::new(exam_id, oj_user_id)?;
            receive(
                tx,
                &key,
                connection_id,
                ClientMessage::Event { seq, event },
                observed_at,
            )
            .await?;
            Ok(BatchOutput::Ack)
        }
        BatchWrite::CommandResult {
            exam_id,
            oj_user_id,
            connection_id,
            command_id,
            name,
            status,
            error_code,
        } => {
            let key = StudentKey::new(exam_id, oj_user_id)?;
            commands::record(tx, key, connection_id, command_id, name, status, error_code).await?;
            Ok(BatchOutput::Ack)
        }
        BatchWrite::Admin {
            actor,
            op,
            last_seen_at,
        } => admin::mutate(tx, service, &actor, &op, last_seen_at)
            .await
            .map(BatchOutput::Admin),
    }
}

async fn flush_heartbeats(
    tx: &DatabaseTransaction,
    heartbeats: &[HeartbeatUpdate],
) -> Result<(), DomainError> {
    const HEARTBEAT_CHUNK_SIZE: usize = 200;
    for chunk in heartbeats.chunks(HEARTBEAT_CHUNK_SIZE) {
        let mut placeholders = Vec::with_capacity(chunk.len());
        let mut values = Vec::with_capacity(chunk.len() * 4);
        for update in chunk {
            db::validate_user(update.oj_user_id)?;
            placeholders.push("(?, ?, ?, ?)");
            values.push(Value::from(update.exam_id.to_string()));
            values.push(Value::from(update.oj_user_id));
            values.push(Value::from(update.connection_id.to_string()));
            values.push(Value::from(db::stored_time(update.last_seen_at)?));
        }
        let sql = format!(
            "WITH batch(exam_id, oj_user_id, connection_id, last_seen_at) AS (VALUES {values_list}) \
             UPDATE student_runtimes \
             SET last_seen_at = batch.last_seen_at \
             FROM batch \
             WHERE student_runtimes.exam_id = batch.exam_id \
               AND student_runtimes.oj_user_id = batch.oj_user_id \
               AND student_runtimes.connection_id = batch.connection_id \
               AND student_runtimes.connected = 1 \
               AND EXISTS ( \
                   SELECT 1 FROM enrollments AS en \
                   WHERE en.exam_id = student_runtimes.exam_id \
                     AND en.oj_user_id = student_runtimes.oj_user_id \
               )",
            values_list = placeholders.join(", ")
        );
        tx.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            values,
        ))
        .await?;
    }
    Ok(())
}

async fn connect(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    connection_id: Option<Uuid>,
    hello: &ClientHello,
    now: OffsetDateTime,
) -> Result<ConnectOutcome, DomainError> {
    use runtime::Column;

    let exam = db::enrollment(tx, key).await?;
    let old = Runtime::find(tx, key).await?;
    let mut committed = old
        .as_ref()
        .map(Runtime::state)
        .transpose()?
        .filter(|state| state.execution == ExecutionState::Finished);
    if let Some(state) = committed
        && !commands::has_end_result(tx, key, state.recording).await?
    {
        committed = None;
    }
    if hello.state.execution == ExecutionState::Finished && committed != Some(hello.state) {
        return Err(DomainError::Conflict);
    }
    if let Some(old) = &old {
        restore_connection(tx, key, old, hello, now).await?;
    }
    let ack_event_through = old.map_or(0, |runtime| runtime.ack_event_through);
    let state = committed.unwrap_or(hello.state);

    runtime::Entity::insert(runtime::ActiveModel {
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        device_id: Set(hello.device_id.to_string()),
        instance_id: Set(hello.instance_id.to_string()),
        connection_id: Set(connection_id.map(|id| id.to_string())),
        connected: Set(true),
        execution: Set(db::enum_name(state.execution)?),
        recording: Set(db::enum_name(state.recording)?),
        last_seen_at: Set(db::stored_time(now)?),
        ack_event_through: Set(ack_event_through),
    })
    .on_conflict(
        OnConflict::columns([Column::ExamId, Column::OjUserId])
            .update_columns([
                Column::DeviceId,
                Column::InstanceId,
                Column::ConnectionId,
                Column::Connected,
                Column::Execution,
                Column::Recording,
                Column::LastSeenAt,
            ])
            .to_owned(),
    )
    .exec_without_returning(tx)
    .await?;

    let (start_at, end_at) = exam.times()?;
    let phase = db::phase(start_at, end_at, now);
    // A client that missed the ACK must be able to report the end result again.
    let command_state = if hello.state.execution == ExecutionState::Finalizing {
        hello.state
    } else {
        state
    };
    let commands = commands::due(command_state, start_at, end_at, now)
        .into_iter()
        .collect();
    Ok(ConnectOutcome {
        sync: ServerMessage::Sync {
            server_time: now,
            phase,
            start_at,
            end_at,
            recording_policy: exam.policy()?,
            ack_event_through,
        },
        commands,
    })
}

async fn restore_connection(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    old: &Runtime,
    hello: &ClientHello,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let old_execution = old.state()?.execution;
    if old_execution != ExecutionState::Finished {
        if old.connected {
            events::save_server_incident(
                tx,
                key,
                "connection_lost",
                now,
                Some(DisconnectReason::Replaced.detail()),
            )
            .await?;
        }
        events::save_server_incident(tx, key, "connection_restored", now, None).await?;
    }
    if old.instance_id != hello.instance_id.to_string()
        && matches!(
            old_execution,
            ExecutionState::Recording | ExecutionState::Finalizing
        )
    {
        events::save_server_incident(tx, key, "process_restarted", now, None).await?;
    }
    Ok(())
}

async fn receive(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    connection_id: Option<Uuid>,
    message: ClientMessage,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let runtime = Runtime::connected_generation(tx, key, connection_id).await?;
    match message {
        ClientMessage::Heartbeat => {
            runtime::Entity::update(runtime::ActiveModel {
                exam_id: Set(key.exam.clone()),
                oj_user_id: Set(key.user),
                last_seen_at: Set(db::stored_time(now)?),
                ..Default::default()
            })
            .exec_without_returning(tx)
            .await?;
        }
        ClientMessage::Status { state } => {
            validate_state(state)?;
            let execution = runtime.state()?.execution;
            if execution != ExecutionState::Finished && state.execution != ExecutionState::Finished
            {
                if matches!(
                    (execution, state.execution),
                    (ExecutionState::Recording, ExecutionState::Ready)
                        | (
                            ExecutionState::Finalizing,
                            ExecutionState::Ready | ExecutionState::Recording
                        )
                ) {
                    return Err(DomainError::InvalidInput);
                }
                update_state(tx, key, state).await?;
            }
        }
        ClientMessage::Event { seq, event } => {
            events::receive(tx, key, &runtime, seq, event, now).await?;
        }
        _ => return Err(DomainError::InvalidInput),
    }
    Ok(())
}

pub(crate) async fn update_state(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    state: ClientState,
) -> Result<(), DomainError> {
    runtime::Entity::update(runtime::ActiveModel {
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        execution: Set(db::enum_name(state.execution)?),
        recording: Set(db::enum_name(state.recording)?),
        ..Default::default()
    })
    .exec_without_returning(tx)
    .await?;
    Ok(())
}

pub(crate) async fn disconnect(
    tx: &DatabaseTransaction,
    runtime: &Runtime,
    reason: DisconnectReason,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    disconnect_with_last_seen(tx, runtime, reason, now, None).await
}

pub(crate) async fn disconnect_with_last_seen(
    tx: &DatabaseTransaction,
    runtime: &Runtime,
    reason: DisconnectReason,
    now: OffsetDateTime,
    last_seen_at: Option<OffsetDateTime>,
) -> Result<(), DomainError> {
    if !runtime.connected {
        return Ok(());
    }
    let key = runtime.key()?;
    if runtime.state()?.execution != ExecutionState::Finished {
        events::save_server_incident(tx, &key, "connection_lost", now, Some(reason.detail()))
            .await?;
    }
    let mut active = runtime::ActiveModel {
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        connected: Set(false),
        ..Default::default()
    };
    if let Some(last_seen) = last_seen_at {
        let existing = db::parse_time(&runtime.last_seen_at)?;
        if last_seen > existing {
            active.last_seen_at = Set(db::stored_time(last_seen)?);
        }
    }
    runtime::Entity::update(active)
        .exec_without_returning(tx)
        .await?;
    Ok(())
}

#[derive(Debug, FromQueryResult)]
struct DueScheduleRow {
    exam_id: String,
    oj_user_id: i64,
    connection_id: Option<String>,
    execution: String,
    recording: String,
    start_at: String,
    end_at: String,
}

pub(crate) fn build_due_schedule_query() -> sea_orm::sea_query::SelectStatement {
    let r = runtime::Entity;
    let en = enrollment::Entity;
    let e = exam_table::Entity;
    Query::select()
        .column((r, runtime::Column::ExamId))
        .column((r, runtime::Column::OjUserId))
        .column((r, runtime::Column::ConnectionId))
        .column((r, runtime::Column::Execution))
        .column((r, runtime::Column::Recording))
        .column((e, exam_table::Column::StartAt))
        .column((e, exam_table::Column::EndAt))
        .from(r)
        .join(
            JoinType::InnerJoin,
            en,
            Expr::col((en, enrollment::Column::ExamId))
                .equals((r, runtime::Column::ExamId))
                .and(
                    Expr::col((en, enrollment::Column::OjUserId))
                        .equals((r, runtime::Column::OjUserId)),
                ),
        )
        .join(
            JoinType::InnerJoin,
            e,
            Expr::col((e, exam_table::Column::ExamId)).equals((r, runtime::Column::ExamId)),
        )
        .and_where(Expr::col((r, runtime::Column::Connected)).eq(true))
        .order_by((r, runtime::Column::ExamId), Order::Asc)
        .order_by((r, runtime::Column::OjUserId), Order::Asc)
        .to_owned()
}

async fn due_commands(
    db: &impl ConnectionTrait,
    now: OffsetDateTime,
) -> Result<Vec<ScheduledCommand>, DomainError> {
    let query = build_due_schedule_query();
    let statement = DbBackend::Sqlite.build(&query);
    let rows = DueScheduleRow::find_by_statement(statement).all(db).await?;
    let mut commands = Vec::new();
    for row in rows {
        let exam_id = db::parse_uuid(&row.exam_id)?;
        db::validate_user(row.oj_user_id)?;
        let connection_id = row
            .connection_id
            .as_deref()
            .map(db::parse_uuid)
            .transpose()?;
        let state = ClientState {
            execution: db::parse_enum(&row.execution)?,
            recording: db::parse_enum(&row.recording)?,
        };
        let start_at = db::parse_time(&row.start_at)?;
        let end_at = db::parse_time(&row.end_at)?;
        if let Some(command) = commands::due(state, start_at, end_at, now) {
            commands.push(ScheduledCommand {
                exam_id,
                oj_user_id: row.oj_user_id,
                connection_id,
                state,
                start_at,
                end_at,
                command,
            });
        }
    }
    Ok(commands)
}

fn validate_state(state: ClientState) -> Result<(), DomainError> {
    use ExecutionState::{Finalizing, Finished, Ready, Recording};
    use RecordingState::{Capturing, Complete, Failed, Idle};

    if matches!(
        (state.execution, state.recording),
        (Ready, Idle)
            | (Recording, Capturing | Failed)
            | (Finalizing, RecordingState::Finalizing | Complete | Failed)
            | (Finished, Complete | Failed)
    ) {
        Ok(())
    } else {
        Err(DomainError::InvalidInput)
    }
}
