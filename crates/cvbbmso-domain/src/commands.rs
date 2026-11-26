use cvbbmso_protocol::{
    ClientState, CommandName, CommandStatus, ExamPhase, ExecutionState, RecordingState,
    ServerMessage, error_code,
};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QuerySelect, Set};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    DomainError,
    db::{self, Runtime, StudentKey},
    entity::command::{ActiveModel, Column, Entity},
    events,
    exam::update_state,
};

pub(crate) fn due(
    state: ClientState,
    start_at: OffsetDateTime,
    end_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Option<ServerMessage> {
    let (name, effective_at) = match (db::phase(start_at, end_at, now), state.execution) {
        (ExamPhase::Running, ExecutionState::Ready) => (CommandName::StartExam, start_at),
        (ExamPhase::Ended, ExecutionState::Recording | ExecutionState::Finalizing) => {
            (CommandName::EndExam, end_at)
        }
        _ => return None,
    };
    Some(ServerMessage::Command {
        command_id: Uuid::new_v4(),
        name,
        effective_at,
    })
}

#[derive(Deserialize, Serialize)]
struct CommandResult {
    status: CommandStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
}

pub(crate) async fn has_end_result(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    recording: RecordingState,
) -> Result<bool, DomainError> {
    let expected = end_status(recording);
    let results = Entity::find()
        .filter(Column::ExamId.eq(key.exam.as_str()))
        .filter(Column::OjUserId.eq(key.user))
        .filter(Column::Name.eq("end_exam"))
        .select_only()
        .column(Column::Result)
        .into_tuple::<String>()
        .all(tx)
        .await?;
    for result in results {
        if db::decode::<CommandResult>(&result)?.status == expected {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) async fn record(
    tx: &DatabaseTransaction,
    key: StudentKey,
    connection_id: Option<Uuid>,
    command_id: Uuid,
    name: CommandName,
    status: CommandStatus,
    error_code: Option<String>,
) -> Result<(), DomainError> {
    if !valid_result(name, status, error_code.as_deref()) {
        return Err(DomainError::Conflict);
    }
    let runtime = Runtime::connected_generation(tx, &key, connection_id).await?;
    let state = runtime.state()?;
    if name == CommandName::EndExam {
        if state.execution == ExecutionState::Finished
            && status != end_status(state.recording)
            && has_end_result(tx, &key, state.recording).await?
        {
            return Err(DomainError::Conflict);
        }
        if events::has_gap(tx, &key, runtime.ack_event_through).await? {
            return Err(DomainError::Conflict);
        }
        if status == CommandStatus::Completed
            && events::recording(tx, &key)
                .await?
                .is_none_or(|(seq, _)| seq > runtime.ack_event_through)
        {
            return Err(DomainError::Conflict);
        }
    }

    Entity::insert(ActiveModel {
        command_id: Set(command_id.to_string()),
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        name: Set(db::enum_name(name)?),
        result: Set(db::encode(&CommandResult { status, error_code })?),
    })
    .exec_without_returning(tx)
    .await?;
    if let Some(state) = resulting_state(name, status, state) {
        update_state(tx, &key, state).await?;
    }
    Ok(())
}

fn end_status(recording: RecordingState) -> CommandStatus {
    if recording == RecordingState::Complete {
        CommandStatus::Completed
    } else {
        CommandStatus::Failed
    }
}

fn valid_result(name: CommandName, status: CommandStatus, code: Option<&str>) -> bool {
    match name {
        CommandName::StartExam | CommandName::EndExam => {}
        _ => return false,
    }
    matches!(
        (name, status, code),
        (_, CommandStatus::Completed, None)
            | (
                _,
                CommandStatus::Failed,
                Some(error_code::INTERNAL_ERROR | error_code::COMMAND_STATE_CONFLICT),
            )
            | (
                CommandName::StartExam,
                CommandStatus::Failed,
                Some(error_code::RECORDING_START_FAILED),
            )
            | (
                CommandName::EndExam,
                CommandStatus::Failed,
                Some(error_code::RECORDING_FINISH_FAILED),
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
