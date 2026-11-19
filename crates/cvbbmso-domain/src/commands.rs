use cvbbmso_protocol::{
    ClientState, CommandName, CommandStatus, ExamPhase, ExecutionState, RecordingState,
    ServerMessage, error_code,
};
use sea_orm::{ConnectionTrait, DatabaseTransaction, FromQueryResult};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    DomainError,
    db::{self, Runtime, StoredExam, StudentKey, sql},
    exam::update_state,
};

pub(crate) async fn issue(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    connection_id: &str,
    state: ClientState,
    exam: &StoredExam,
    now: OffsetDateTime,
) -> Result<Option<ServerMessage>, DomainError> {
    let (start_at, end_at) = exam.times()?;
    let (name, effective_at) = match (db::phase(start_at, end_at, now), state.execution) {
        (ExamPhase::Running, ExecutionState::Ready) => (CommandName::StartExam, start_at),
        (ExamPhase::Ended, ExecutionState::Recording | ExecutionState::Finalizing) => {
            (CommandName::EndExam, end_at)
        }
        _ => return Ok(None),
    };
    let command_id = Uuid::new_v4();
    let inserted = tx
        .execute_raw(sql!(
            "INSERT INTO commands (
                 command_id, exam_id, oj_user_id, connection_id, name, effective_at, created_at, lifecycle
             ) VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')
             ON CONFLICT(exam_id, oj_user_id, connection_id, name) WHERE lifecycle = 'pending'
             DO NOTHING",
            command_id.to_string(), key.exam.as_str(), key.user, connection_id, db::enum_name(name)?,
            db::stored_time(effective_at)?, db::stored_time(now)?,
        ))
        .await?;
    Ok(
        (inserted.rows_affected() != 0).then_some(ServerMessage::Command {
            command_id,
            name,
            effective_at,
        }),
    )
}

pub(crate) async fn retire_pending(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    state: ClientState,
) -> Result<(), DomainError> {
    // A failed capture still means the client acted on the start command.
    let started = matches!(
        state.execution,
        ExecutionState::Recording | ExecutionState::Finalizing
    ) || state.execution == ExecutionState::Finished;
    let ended = state.execution == ExecutionState::Finished;
    tx.execute_raw(sql!(
        "UPDATE commands SET lifecycle = CASE
             WHEN (name = 'start_exam' AND ?) OR (name = 'end_exam' AND ?) THEN 'inferred'
             ELSE 'superseded'
         END WHERE exam_id = ? AND oj_user_id = ? AND lifecycle = 'pending'",
        started,
        ended,
        key.exam.as_str(),
        key.user,
    ))
    .await?;
    Ok(())
}

#[derive(FromQueryResult)]
struct Command {
    exam_id: String,
    oj_user_id: i64,
    connection_id: String,
    name: String,
    lifecycle: String,
    result_status: Option<String>,
    result_error_code: Option<String>,
}

pub(crate) async fn result(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    command_id: Uuid,
    status: CommandStatus,
    error_code: Option<String>,
) -> Result<ServerMessage, DomainError> {
    let id = command_id.to_string();
    let command = Command::find_by_statement(sql!(
        "SELECT * FROM commands WHERE command_id = ?",
        id.as_str(),
    ))
    .one(tx)
    .await?
    .filter(|command| command.exam_id == key.exam && command.oj_user_id == key.user)
    .ok_or(DomainError::Conflict)?;

    if !valid_result(status, error_code.as_deref()) {
        return Err(DomainError::Conflict);
    }
    let status_name = db::enum_name(status)?;
    let ack = ServerMessage::CommandAck { command_id };
    if let Some(previous) = command.result_status {
        return if previous == status_name && command.result_error_code == error_code {
            Ok(ack)
        } else {
            Err(DomainError::Conflict)
        };
    }
    let runtime = Runtime::get(tx, key).await?;
    if !runtime.connected
        || command.lifecycle != "pending"
        || command.connection_id != runtime.connection_id
    {
        return Err(DomainError::Conflict);
    }

    tx.execute_raw(sql!(
        "UPDATE commands SET lifecycle = 'reported', result_status = ?, result_error_code = ?
         WHERE command_id = ?",
        status_name,
        error_code,
        id,
    ))
    .await?;
    if let Some(state) = resulting_state(db::parse_enum(&command.name)?, status, runtime.state()?) {
        update_state(tx, key, state).await?;
    }
    Ok(ack)
}

fn valid_result(status: CommandStatus, code: Option<&str>) -> bool {
    matches!(
        (status, code),
        (CommandStatus::Completed, None)
            | (
                CommandStatus::Failed,
                Some(
                    error_code::INTERNAL_ERROR
                        | error_code::COMMAND_STATE_CONFLICT
                        | error_code::RECORDING_START_FAILED
                        | error_code::RECORDING_FINISH_FAILED
                ),
            )
    )
}

fn resulting_state(
    name: CommandName,
    status: CommandStatus,
    state: ClientState,
) -> Option<ClientState> {
    let complete = status == CommandStatus::Completed;
    match name {
        CommandName::StartExam if state.execution == ExecutionState::Ready => Some(ClientState {
            execution: ExecutionState::Recording,
            recording: if complete {
                RecordingState::Capturing
            } else {
                RecordingState::Failed
            },
        }),
        CommandName::EndExam => Some(ClientState {
            execution: ExecutionState::Finished,
            recording: if complete {
                RecordingState::Complete
            } else {
                RecordingState::Failed
            },
        }),
        _ => None,
    }
}
