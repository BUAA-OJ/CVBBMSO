use cvbbmso_protocol::{ExamEvent, RecordingManifest, SegmentMeta};
use sea_orm::{ConnectionTrait, DatabaseTransaction, FromQueryResult};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    DomainError,
    db::{self, Runtime, StudentKey, sql},
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
    if let Some(existing) = tx
        .query_one_raw(sql!(
            "SELECT event FROM events WHERE exam_id = ? AND oj_user_id = ? AND seq = ?",
            key.exam.as_str(),
            key.user,
            encoded_seq.clone(),
        ))
        .await?
    {
        let existing: ExamEvent = db::decode(&existing.try_get::<String>("", "event")?)?;
        return if existing == event {
            Ok(())
        } else {
            Err(DomainError::InvalidInput)
        };
    }

    tx.execute_raw(sql!(
        "INSERT INTO events (exam_id, oj_user_id, seq, event, received_at) VALUES (?, ?, ?, ?, ?)",
        key.exam.as_str(),
        key.user,
        encoded_seq.clone(),
        db::encode(&event)?,
        db::stored_time(now)?,
    ))
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
            tx.execute_raw(sql!(
                "INSERT INTO recording_manifests (exam_id, oj_user_id, source_seq, manifest)
                 VALUES (?, ?, ?, ?) ON CONFLICT (exam_id, oj_user_id) DO UPDATE SET
                 source_seq = excluded.source_seq, manifest = excluded.manifest
                 WHERE excluded.source_seq > recording_manifests.source_seq",
                key.exam.as_str(),
                key.user,
                encoded_seq,
                db::encode(&manifest)?,
            ))
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

#[derive(FromQueryResult)]
struct Sequence {
    seq: Vec<u8>,
}

async fn advance_ack(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    mut count: u64,
) -> Result<(), DomainError> {
    // Scan bounded batches so a large out-of-order backlog does not load full events
    // or allocate unbounded memory. The BLOB ordering preserves the unsigned order.
    loop {
        let sequences = Sequence::find_by_statement(sql!(
            "SELECT seq FROM events WHERE exam_id = ? AND oj_user_id = ? AND seq > ?
             ORDER BY seq LIMIT 256",
            key.exam.as_str(),
            key.user,
            db::encode_seq(count),
        ))
        .all(tx)
        .await?;
        let full_batch = sequences.len() == 256;
        let mut continuous = true;
        for Sequence { seq } in sequences {
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
    tx.execute_raw(sql!(
        "UPDATE student_runtimes SET ack_event_through = ? WHERE exam_id = ? AND oj_user_id = ?",
        db::encode_seq(count),
        key.exam.as_str(),
        key.user,
    ))
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
    tx.execute_raw(sql!(
        "INSERT INTO incidents (incident_id, exam_id, oj_user_id, source, kind, occurred_at, detail, event_seq)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        id.as_str(), key.exam.as_str(), key.user, evidence.source, evidence.kind,
        evidence.occurred_at, evidence.detail, evidence.seq,
    ))
    .await?;
    Ok(id)
}
