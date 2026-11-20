use cvbbmso_protocol::{ExamEvent, RecordingManifest, SegmentMeta};
use sea_orm::{
    ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    sea_query::OnConflict,
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    DomainError,
    db::{self, Runtime, StudentKey},
    entity::{event as stored_event, incident, manifest as stored_manifest, runtime},
};

pub(crate) async fn receive(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    runtime: &Runtime,
    seq: u64,
    event: ExamEvent,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    if seq == 0 {
        return Err(DomainError::InvalidInput);
    }
    let event = validated(event)?;
    let encoded_seq = db::encode_seq(seq);
    if let Some(existing) =
        stored_event::Entity::find_by_id((key.exam.clone(), key.user, encoded_seq.clone()))
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

    stored_event::Entity::insert(stored_event::ActiveModel {
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        seq: Set(encoded_seq.clone()),
        event: Set(db::encode(&event)?),
        received_at: Set(db::stored_time(now)?),
    })
    .exec_without_returning(tx)
    .await?;
    match event {
        ExamEvent::Incident(incident) => {
            save_incident(
                tx,
                key,
                Evidence {
                    source: "client",
                    kind: &db::enum_name(incident.code)?,
                    occurred_at: db::input_time(incident.occurred_at)?,
                    detail: incident.detail.as_deref(),
                    seq: Some(encoded_seq),
                },
            )
            .await?;
        }
        ExamEvent::RecordingFinalized(manifest) => {
            use stored_manifest::Column;
            stored_manifest::Entity::insert(stored_manifest::ActiveModel {
                exam_id: Set(key.exam.clone()),
                oj_user_id: Set(key.user),
                source_seq: Set(encoded_seq.clone()),
                manifest: Set(db::encode(&manifest)?),
            })
            .on_conflict(
                OnConflict::columns([Column::ExamId, Column::OjUserId])
                    .update_columns([Column::SourceSeq, Column::Manifest])
                    .action_and_where(Column::SourceSeq.lt(encoded_seq))
                    .to_owned(),
            )
            .exec_without_returning(tx)
            .await?;
        }
        _ => {}
    }

    let acknowledged = db::decode_seq(&runtime.ack_event_through)?;
    if acknowledged.checked_add(1) == Some(seq) {
        advance_ack(tx, key, seq).await?;
    }
    Ok(())
}

fn validated(event: ExamEvent) -> Result<ExamEvent, DomainError> {
    match event {
        ExamEvent::SegmentFinalized(segment) => {
            validate_segment(segment).map(ExamEvent::SegmentFinalized)
        }
        ExamEvent::RecordingFinalized(manifest) => {
            let segments = manifest
                .segments
                .into_iter()
                .map(validate_segment)
                .collect::<Result<_, _>>()?;
            RecordingManifest::new(segments, manifest.merged_sha256)
                .map(ExamEvent::RecordingFinalized)
                .map_err(|_| DomainError::InvalidInput)
        }
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
    mut count: u64,
) -> Result<(), DomainError> {
    // Scan bounded batches so a large out-of-order backlog does not load full events
    // or allocate unbounded memory. The BLOB ordering preserves the unsigned order.
    loop {
        use stored_event::Column;
        let sequences = stored_event::Entity::find()
            .select_only()
            .column(Column::Seq)
            .filter(Column::ExamId.eq(key.exam.as_str()))
            .filter(Column::OjUserId.eq(key.user))
            .filter(Column::Seq.gt(db::encode_seq(count)))
            .order_by_asc(Column::Seq)
            .limit(256)
            .into_tuple::<Vec<u8>>()
            .all(tx)
            .await?;
        let full_batch = sequences.len() == 256;
        let mut continuous = true;
        for seq in sequences {
            let next = db::decode_seq(&seq)?;
            if count.checked_add(1) != Some(next) {
                continuous = false;
                break;
            }
            count = next;
        }
        if !full_batch || !continuous || count == u64::MAX {
            break;
        }
    }
    runtime::Entity::update(runtime::ActiveModel {
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        ack_event_through: Set(db::encode_seq(count)),
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
) -> Result<String, DomainError> {
    save_incident(
        tx,
        key,
        Evidence {
            source: "server",
            kind,
            occurred_at: db::stored_time(now)?,
            detail,
            seq: None,
        },
    )
    .await
}

struct Evidence<'a> {
    source: &'static str,
    kind: &'a str,
    occurred_at: String,
    detail: Option<&'a str>,
    seq: Option<Vec<u8>>,
}

async fn save_incident(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    evidence: Evidence<'_>,
) -> Result<String, DomainError> {
    let id = Uuid::new_v4().to_string();
    incident::Entity::insert(incident::ActiveModel {
        incident_id: Set(id.clone()),
        exam_id: Set(key.exam.clone()),
        oj_user_id: Set(key.user),
        source: Set(evidence.source.into()),
        kind: Set(evidence.kind.into()),
        occurred_at: Set(evidence.occurred_at),
        detail: Set(evidence.detail.map(String::from)),
        event_seq: Set(evidence.seq),
        ..Default::default()
    })
    .exec_without_returning(tx)
    .await?;
    Ok(id)
}
