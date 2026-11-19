use cvbbmso_protocol::{
    ClientHello, ClientMessage, ClientState, ExamSummary, ExecutionState, RecordingState,
    ServerMessage,
};
use sea_orm::{ConnectionTrait, DatabaseTransaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AdminService, DomainError, Service, commands,
    db::{self, Runtime, StudentKey, sql},
    events,
};

/// A committed connection snapshot and the commands issued on that connection.
#[derive(Debug)]
pub struct ConnectOutcome {
    /// The authoritative `ServerMessage::Sync` snapshot.
    pub sync: ServerMessage,
    /// Commands to send after the snapshot.
    pub commands: Vec<ServerMessage>,
}

/// A committed command and the student to whom it must be delivered.
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
    /// The transport closed or a service restart discovered a stale connection.
    Closed,
    /// No application heartbeat arrived by the deadline.
    HeartbeatTimeout,
    /// A newer connection took over the same exam and student.
    Replaced,
}

impl DisconnectReason {
    fn detail(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::HeartbeatTimeout => "heartbeat_timeout",
            Self::Replaced => "replaced",
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
                    disconnect(tx, &runtime, DisconnectReason::Closed, now).await?;
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

    /// Checks whether the Accoding user is enrolled in the exam.
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
    ) -> Result<(), DomainError> {
        let key = StudentKey::new(exam_id, oj_user_id)?;
        db::transaction(&self.0.database, |tx| {
            Box::pin(async move { db::enrollment(tx, &key).await.map(|_| ()) })
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

    /// Commits an application message from the current connection.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, a missing runtime, a command conflict,
    /// or a failed database transaction.
    pub async fn receive(
        &self,
        exam_id: Uuid,
        oj_user_id: i64,
        message: ClientMessage,
    ) -> Result<Vec<ServerMessage>, DomainError> {
        let key = StudentKey::new(exam_id, oj_user_id)?;
        let now = self.0.now();
        db::write(&self.0.database, |tx| {
            let key = key.clone();
            let message = message.clone();
            Box::pin(async move { receive(tx, &key, message, now).await })
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
        let key = StudentKey::new(exam_id, oj_user_id)?;
        let now = self.0.now();
        db::write(&self.0.database, |tx| {
            let key = key.clone();
            Box::pin(async move {
                let runtime = Runtime::get(tx, &key).await?;
                disconnect(tx, &runtime, reason, now).await
            })
        })
        .await
    }

    /// Generates and commits the commands now due for connected students.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the schedule cannot be applied.
    pub async fn due_commands(&self) -> Result<Vec<DirectedCommand>, DomainError> {
        let now = self.0.now();
        db::write(&self.0.database, |tx| Box::pin(due_commands(tx, now))).await
    }
}

async fn connect(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    hello: &ClientHello,
    now: OffsetDateTime,
) -> Result<ConnectOutcome, DomainError> {
    let exam = db::enrollment(tx, key).await?;
    let mut ack_event_through = 0;
    if let Some(old) = Runtime::find(tx, key).await? {
        restore_connection(tx, key, &old, hello, now).await?;
        ack_event_through = db::decode_seq(&old.ack_event_through)?;
    }

    let connection_id = Uuid::new_v4().to_string();
    let instance_id = hello.instance_id.to_string();
    tx.execute_raw(sql!(
        "INSERT INTO student_runtimes (
             exam_id, oj_user_id, device_id, instance_id, platform, connection_id,
             connected, execution, recording, last_seen_at, ack_event_through
         ) VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?)
         ON CONFLICT(exam_id, oj_user_id) DO UPDATE SET
             device_id = excluded.device_id, instance_id = excluded.instance_id,
             platform = excluded.platform, connection_id = excluded.connection_id,
             connected = 1, execution = excluded.execution, recording = excluded.recording,
             last_seen_at = excluded.last_seen_at, lost_incident_id = NULL",
        key.exam.as_str(),
        key.user,
        hello.device_id.to_string(),
        instance_id,
        db::enum_name(hello.platform)?,
        connection_id.as_str(),
        db::enum_name(hello.state.execution)?,
        db::enum_name(hello.state.recording)?,
        db::stored_time(now)?,
        db::encode_seq(ack_event_through),
    ))
    .await?;

    let (start_at, end_at) = exam.times()?;
    let phase = db::phase(start_at, end_at, now);
    let commands = commands::issue(tx, key, &connection_id, hello.state, &exam, now)
        .await?
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
    let replaced = old.connected && old_execution != ExecutionState::Finished;
    if replaced {
        events::save_server_incident(
            tx,
            key,
            "connection_lost",
            now,
            Some(DisconnectReason::Replaced.detail()),
        )
        .await?;
    }
    if replaced || old.lost_incident_id.is_some() {
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
    commands::retire_pending(tx, key, hello.state).await
}

async fn receive(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    message: ClientMessage,
    now: OffsetDateTime,
) -> Result<Vec<ServerMessage>, DomainError> {
    let message = match message {
        ClientMessage::Hello(_) => return Err(DomainError::InvalidInput),
        ClientMessage::CommandResult {
            command_id,
            status,
            error_code,
        } => {
            let ack = commands::result(tx, key, command_id, status, error_code).await?;
            return Ok(vec![ack]);
        }
        message => message,
    };
    let runtime = Runtime::get(tx, key).await?;
    if !runtime.connected {
        return Err(DomainError::NotFound);
    }
    match message {
        ClientMessage::Heartbeat => {
            tx.execute_raw(sql!(
                "UPDATE student_runtimes SET last_seen_at = ? WHERE exam_id = ? AND oj_user_id = ?",
                db::stored_time(now)?,
                key.exam.as_str(),
                key.user,
            ))
            .await?;
        }
        ClientMessage::Status { state } => {
            validate_state(state)?;
            if runtime.state()?.execution != ExecutionState::Finished
                && state.execution != ExecutionState::Finished
            {
                update_state(tx, key, state).await?;
            }
        }
        ClientMessage::Event { seq, event } => {
            events::receive(tx, key, &runtime, seq, event, now).await?;
        }
        _ => return Err(DomainError::InvalidInput),
    }
    Ok(Vec::new())
}

pub(crate) async fn update_state(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    state: ClientState,
) -> Result<(), DomainError> {
    tx.execute_raw(sql!(
        "UPDATE student_runtimes SET execution = ?, recording = ? WHERE exam_id = ? AND oj_user_id = ?",
        db::enum_name(state.execution)?, db::enum_name(state.recording)?, key.exam.as_str(), key.user,
    ))
    .await?;
    Ok(())
}

async fn disconnect(
    tx: &DatabaseTransaction,
    runtime: &Runtime,
    reason: DisconnectReason,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    if !runtime.connected {
        return Ok(());
    }
    let key = runtime.key()?;
    let lost_id = if runtime.state()?.execution == ExecutionState::Finished {
        None
    } else {
        Some(
            events::save_server_incident(tx, &key, "connection_lost", now, Some(reason.detail()))
                .await?,
        )
    };
    tx.execute_raw(sql!(
        "UPDATE student_runtimes SET connected = 0, lost_incident_id = ?
         WHERE exam_id = ? AND oj_user_id = ?",
        lost_id,
        key.exam.as_str(),
        key.user,
    ))
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
        let exam = db::exam(tx, &key.exam).await.map_err(|error| match error {
            DomainError::NotFound => {
                DomainError::storage(sea_orm::DbErr::RecordNotFound(key.exam.clone()))
            }
            error => error,
        })?;
        if let Some(command) = commands::issue(
            tx,
            &key,
            &runtime.connection_id,
            runtime.state()?,
            &exam,
            now,
        )
        .await?
        {
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
