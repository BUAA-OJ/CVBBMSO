use cvbbmso_protocol::{
    ClientHello, ClientMessage, ClientState, CommandName, CommandStatus, ExamSummary,
    ExecutionState, RecordingState, ServerMessage,
};
use sea_orm::{DatabaseTransaction, EntityTrait, Set, sea_query::OnConflict};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AdminService, DomainError, Service, commands,
    db::{self, Runtime, StudentKey},
    entity::runtime,
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

    /// Transitions connections left online by a previous server to offline.
    ///
    /// Call once before accepting connections after server startup.
    ///
    /// # Errors
    ///
    /// Returns a storage error if recovery cannot be committed.
    pub async fn recover_online(&self) -> Result<(), DomainError> {
        let now = self.0.now();
        db::write(&self.0.database, |tx| {
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
        db::exams(&self.0.database, Some(oj_user_id), self.0.now()).await
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
        db::transaction(&self.0.database, |tx| {
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
        let key = StudentKey::new(exam_id, oj_user_id)?;
        db::transaction(&self.0.database, |tx| {
            Box::pin(async move { Runtime::connected(tx, &key).await.map(|_| ()) })
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
        db::write(&self.0.database, |tx| {
            let key = key.clone();
            let hello = hello.clone();
            Box::pin(async move { connect(tx, &key, &hello, now).await })
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
        db::transaction(&self.0.database, |tx| {
            Box::pin(async move { receive(tx, &key, message, now).await })
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
        db::transaction(&self.0.database, |tx| {
            Box::pin(commands::record(
                tx, key, command_id, name, status, error_code,
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
        db::transaction(&self.0.database, |tx| {
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
        let now = self.0.now();
        db::transaction(&self.0.database, |tx| Box::pin(due_commands(tx, now))).await
    }
}

async fn connect(
    tx: &DatabaseTransaction,
    key: &StudentKey,
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
    message: ClientMessage,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let runtime = Runtime::connected(tx, key).await?;
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
    if !runtime.connected {
        return Ok(());
    }
    let key = runtime.key()?;
    if runtime.state()?.execution != ExecutionState::Finished {
        events::save_server_incident(tx, &key, "connection_lost", now, Some(reason.detail()))
            .await?;
    }
    runtime::Entity::update(runtime::ActiveModel {
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        connected: Set(false),
        ..Default::default()
    })
    .exec_without_returning(tx)
    .await?;
    Ok(())
}

async fn due_commands(
    tx: &DatabaseTransaction,
    now: OffsetDateTime,
) -> Result<Vec<DirectedCommand>, DomainError> {
    let mut commands = Vec::new();
    for runtime in Runtime::online(tx).await? {
        let key = runtime.key()?;
        let exam = match db::enrollment(tx, &key).await {
            Ok(exam) => exam,
            Err(DomainError::NotEnrolled) => continue,
            Err(error) => return Err(error),
        };
        let (start_at, end_at) = exam.times()?;
        if let Some(command) = commands::due(runtime.state()?, start_at, end_at, now) {
            commands.push(DirectedCommand {
                exam_id: key.exam_id(),
                oj_user_id: key.user,
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
