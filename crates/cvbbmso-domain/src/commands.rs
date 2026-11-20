use cvbbmso_protocol::{
    ClientState, CommandName, CommandStatus, ExamPhase, ExecutionState, RecordingState,
    ServerMessage, error_code,
};
use sea_orm::{
    ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set,
    sea_query::{Expr, ExprTrait, OnConflict},
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    DomainError,
    db::{self, Runtime, StoredExam, StudentKey},
    entity::command::{ActiveModel, Column, Entity},
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
    let inserted = Entity::insert(ActiveModel {
        command_id: Set(command_id.to_string()),
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        connection_id: Set(connection_id.into()),
        name: Set(db::enum_name(name)?),
        effective_at: Set(db::stored_time(effective_at)?),
        created_at: Set(db::stored_time(now)?),
        lifecycle: Set("pending".into()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([
            Column::ExamId,
            Column::OjUserId,
            Column::ConnectionId,
            Column::Name,
        ])
        // SQLite matches the conflict target to the partial index before binding values.
        .target_and_where(Expr::col(Column::Lifecycle).eq(Expr::Constant("pending".into())))
        .do_nothing()
        .to_owned(),
    )
    .exec_without_returning(tx)
    .await?;
    Ok((inserted != 0).then_some(ServerMessage::Command {
        command_id,
        name,
        effective_at,
    }))
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
    let inferred = Column::Name
        .eq(db::enum_name(CommandName::StartExam)?)
        .and(Expr::val(started))
        .or(Column::Name
            .eq(db::enum_name(CommandName::EndExam)?)
            .and(Expr::val(ended)));
    Entity::update_many()
        .col_expr(
            Column::Lifecycle,
            Expr::case(inferred, "inferred")
                .finally("superseded")
                .into(),
        )
        .filter(Column::ExamId.eq(key.exam.as_str()))
        .filter(Column::OjUserId.eq(key.user))
        .filter(Column::Lifecycle.eq("pending"))
        .exec(tx)
        .await?;
    Ok(())
}

pub(crate) async fn result(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    command_id: Uuid,
    status: CommandStatus,
    error_code: Option<String>,
) -> Result<ServerMessage, DomainError> {
    let command = Entity::find_by_id(command_id.to_string())
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

    Entity::update(ActiveModel {
        command_id: Set(command.command_id),
        lifecycle: Set("reported".into()),
        result_status: Set(Some(status_name)),
        result_error_code: Set(error_code),
        ..Default::default()
    })
    .exec_without_returning(tx)
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
