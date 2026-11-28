use cvbbmso_protocol::{ExamEvent, RecordingSummary, SegmentMeta};
use sea_orm::{
    ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Select,
    Set,
    sea_query::{Expr, ExprTrait, Func},
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    DomainError,
    db::{self, Runtime, StudentKey},
    entity::{event as stored_event, incident, runtime},
};

pub(crate) async fn receive(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    runtime: &Runtime,
    seq: i64,
    event: ExamEvent,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    if seq <= 0 {
        return Err(DomainError::InvalidInput);
    }
    let event = validated(event)?;
    if let Some(existing) = student_events(key)
        .filter(stored_event::Column::Seq.eq(seq))
        .one(tx)
        .await?
    {
        let existing: ExamEvent = db::decode(&existing.event)?;
        return if existing == event {
            Ok(())
        } else {
            Err(DomainError::InvalidInput)
        };
    }

    if let ExamEvent::RecordingFinalized(ref summary) = event
        && recording(tx, key)
            .await
            .map_err(|error| match error {
                DomainError::Conflict => DomainError::InvalidInput,
                error => error,
            })?
            .is_some_and(|(_, existing)| &existing != summary)
    {
        return Err(DomainError::InvalidInput);
    }

    stored_event::Entity::insert(stored_event::ActiveModel {
        write_id: sea_orm::NotSet,
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        seq: Set(seq),
        event: Set(db::encode(&event)?),
        received_at: Set(db::stored_time(now)?),
    })
    .exec_without_returning(tx)
    .await?;
    if let ExamEvent::Incident(incident) = event {
        save_incident(
            tx,
            key,
            &db::enum_name(incident.code)?,
            db::input_time(incident.occurred_at)?,
            incident.detail,
        )
        .await?;
    }

    if runtime.ack_event_through.checked_add(1) == Some(seq) {
        advance_ack(tx, key, seq).await?;
    }
    Ok(())
}

fn student_events(key: &StudentKey) -> Select<stored_event::Entity> {
    use stored_event::Column;
    stored_event::Entity::find()
        .filter(Column::ExamId.eq(key.exam.as_str()))
        .filter(Column::OjUserId.eq(key.user))
}

pub(crate) async fn has_gap(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    ack_event_through: i64,
) -> Result<bool, DomainError> {
    use stored_event::Column;
    Ok(student_events(key)
        .select_only()
        .column(Column::Seq)
        .filter(Column::Seq.gt(ack_event_through))
        .into_tuple::<i64>()
        .one(tx)
        .await?
        .is_some())
}

pub(crate) async fn recording(
    tx: &DatabaseTransaction,
    key: &StudentKey,
) -> Result<Option<(i64, RecordingSummary)>, DomainError> {
    use stored_event::Column;
    let recordings = student_events(key)
        .filter(
            Expr::from(
                Func::cust("json_extract")
                    .arg(Expr::col(Column::Event))
                    .arg("$.kind"),
            )
            .eq("recording_finalized"),
        )
        .select_only()
        .expr(Func::min(Expr::col(Column::Seq)))
        .column(Column::Event)
        .group_by(Column::Event)
        .limit(2)
        .into_tuple::<(i64, String)>()
        .all(tx)
        .await?;
    match recordings.as_slice() {
        [] => Ok(None),
        [(seq, event)] => match db::decode(event)? {
            ExamEvent::RecordingFinalized(summary) => Ok(Some((*seq, summary))),
            _ => Err(DomainError::InvalidInput),
        },
        _ => Err(DomainError::Conflict),
    }
}

fn validated(event: ExamEvent) -> Result<ExamEvent, DomainError> {
    match event {
        ExamEvent::SegmentFinalized(segment) => {
            validate_segment(segment).map(ExamEvent::SegmentFinalized)
        }
        ExamEvent::RecordingFinalized(_) => Ok(event),
        ExamEvent::Incident(incident) => {
            db::input_time(incident.occurred_at)?;
            Ok(ExamEvent::Incident(incident))
        }
        _ => Err(DomainError::InvalidInput),
    }
}

fn validate_segment(segment: SegmentMeta) -> Result<SegmentMeta, DomainError> {
    db::input_time(segment.start_at)?;
    db::input_time(segment.end_at)?;
    SegmentMeta::new(
        segment.index,
        segment.start_at,
        segment.end_at,
        segment.sha256,
    )
    .map_err(|_| DomainError::InvalidInput)
}

async fn advance_ack(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    mut count: i64,
) -> Result<(), DomainError> {
    // Scan bounded batches so a large out-of-order backlog does not load full events
    // or allocate unbounded memory.
    loop {
        use stored_event::Column;
        let sequences = student_events(key)
            .select_only()
            .column(Column::Seq)
            .filter(Column::Seq.gt(count))
            .order_by_asc(Column::Seq)
            .limit(256)
            .into_tuple::<i64>()
            .all(tx)
            .await?;
        let full_batch = sequences.len() == 256;
        let mut continuous = true;
        for next in sequences {
            if count.checked_add(1) != Some(next) {
                continuous = false;
                break;
            }
            count = next;
        }
        if !full_batch || !continuous || count == i64::MAX {
            break;
        }
    }
    runtime::Entity::update(runtime::ActiveModel {
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        ack_event_through: Set(count),
        ..Default::default()
    })
    .exec_without_returning(tx)
    .await?;
    Ok(())
}

pub(crate) async fn save_server_incident(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    kind: &str,
    now: OffsetDateTime,
    detail: Option<&str>,
) -> Result<(), DomainError> {
    save_incident(
        tx,
        key,
        kind,
        db::stored_time(now)?,
        detail.map(String::from),
    )
    .await
}

async fn save_incident(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    kind: &str,
    occurred_at: String,
    detail: Option<String>,
) -> Result<(), DomainError> {
    incident::Entity::insert(incident::ActiveModel {
        incident_id: Set(Uuid::new_v4().to_string()),
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        kind: Set(kind.into()),
        occurred_at: Set(occurred_at),
        detail: Set(detail),
    })
    .exec_without_returning(tx)
    .await?;
    Ok(())
}
