use cvbbmso_protocol::{
    ClientState, CommandName, CommandStatus, ExamPhase, ExecutionState, RecordingState,
    ServerMessage, error_code,
};
use sea_orm::{DatabaseTransaction, EntityTrait, Set};
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    DomainError,
    db::{self, Runtime, StudentKey},
    entity::command::{ActiveModel, Entity},
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

#[derive(Serialize)]
struct CommandResult {
    status: CommandStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
}

pub(crate) async fn record(
    tx: &DatabaseTransaction,
    key: StudentKey,
    command_id: Uuid,
    name: CommandName,
    status: CommandStatus,
    error_code: Option<String>,
) -> Result<(), DomainError> {
    if !valid_result(name, status, error_code.as_deref()) {
        return Err(DomainError::Conflict);
    }
    let runtime = Runtime::get(tx, &key).await?;
    if !runtime.connected {
        return Err(DomainError::Conflict);
    }
    if name == CommandName::EndExam {
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
    if let Some(state) = resulting_state(name, status, runtime.state()?) {
        update_state(tx, &key, state).await?;
    }
    Ok(())
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
