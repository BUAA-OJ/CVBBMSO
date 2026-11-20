use std::collections::BTreeSet;

use cvbbmso_protocol::{
    ExamEvent, ExamSummary, ExecutionState, RecordingManifest, RecordingPolicy, RecordingState,
};
use sea_orm::{
    ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder, QueryTrait, Set,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    DomainError, Service,
    db::{self, Runtime, StudentKey},
    entity::{audit as stored_audit, enrollment, event, exam, incident, manifest, runtime},
};

/// A management operation to perform against the shared exam database.
#[derive(Debug, Clone)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum AdminOperation {
    /// Creates an exam with fixed times and recording settings.
    ExamsCreate {
        title: String,
        start_at: OffsetDateTime,
        end_at: OffsetDateTime,
        policy: RecordingPolicy,
    },
    /// Lists all exams by start time and ID.
    ExamsList,
    /// Looks up a single exam and its policy.
    ExamsShow { exam_id: Uuid },
    /// Enrolls an individual participant.
    ParticipantsAdd { exam_id: Uuid, oj_user_id: i64 },
    /// Removes an enrollment while retaining evidence.
    ParticipantsRemove { exam_id: Uuid, oj_user_id: i64 },
    /// Enrolls a batch, deduplicating IDs and skipping existing enrollments.
    ParticipantsImport {
        exam_id: Uuid,
        oj_user_ids: Vec<i64>,
    },
    /// Lists current enrollments for an exam.
    ParticipantsList { exam_id: Uuid },
    /// Lists runtime records, including those from removed enrollments.
    StudentsStatus { exam_id: Uuid },
    /// Looks up a student's runtime record.
    StudentsShow { exam_id: Uuid, oj_user_id: i64 },
    /// Lists a student's reliable events in ascending sequence order.
    EventsList { exam_id: Uuid, oj_user_id: i64 },
    /// Lists incidents using optional student, kind and inclusive time filters.
    IncidentsList {
        exam_id: Uuid,
        oj_user_id: Option<i64>,
        kind: Option<String>,
        since: Option<OffsetDateTime>,
        until: Option<OffsetDateTime>,
    },
    /// Looks up an incident by ID.
    IncidentsShow { incident_id: Uuid },
    /// Looks up the latest successful recording summary.
    RecordingsShow { exam_id: Uuid, oj_user_id: i64 },
}

/// The committed result of a management operation.
#[derive(Debug)]
#[non_exhaustive]
pub enum AdminOutput {
    /// The ID assigned to the new exam.
    ExamCreated {
        /// The generated ID.
        exam_id: Uuid,
    },
    /// The ordered list of exams.
    Exams(Vec<ExamSummary>),
    /// Summary and recording policy for an exam.
    ExamDetail {
        /// Exam identity, timing and current phase.
        summary: ExamSummary,
        /// Fixed recording parameters.
        policy: RecordingPolicy,
    },
    /// An enrollment was added.
    ParticipantAdded,
    /// An enrollment was removed.
    ParticipantRemoved,
    /// A batch was imported.
    ParticipantsImported {
        /// The number of newly enrolled students.
        count: usize,
    },
    /// Current exam enrollments.
    Participants(Vec<ParticipantRecord>),
    /// All known runtimes for the exam.
    Students(Vec<StudentRuntime>),
    /// A student's current persisted runtime.
    Student(StudentRuntime),
    /// A student's persisted reliable events.
    Events(Vec<EventRecord>),
    /// The matching incidents in occurrence order.
    Incidents(Vec<IncidentRecord>),
    /// A single incident record.
    Incident(IncidentRecord),
    /// The latest successful recording summary.
    Recording(RecordingManifest),
}

/// An active enrollment.
#[derive(Debug)]
pub struct ParticipantRecord {
    /// The enrolled exam.
    pub exam_id: Uuid,
    /// The enrolled Accoding user.
    pub oj_user_id: i64,
}

/// The current persisted state of a student's exam runtime.
#[derive(Debug)]
pub struct StudentRuntime {
    /// The exam in which the student connected.
    pub exam_id: Uuid,
    /// The student's Accoding user ID.
    pub oj_user_id: i64,
    /// Whether the service currently has a live connection.
    pub connected: bool,
    /// The student's authoritative execution state.
    pub execution: ExecutionState,
    /// The last connection or application heartbeat time in UTC.
    pub last_seen_at: Option<OffsetDateTime>,
}

/// A persisted reliable client event.
#[derive(Debug)]
pub struct EventRecord {
    /// The student's exam ID.
    pub exam_id: Uuid,
    /// The student's Accoding user ID.
    pub oj_user_id: i64,
    /// The original event sequence number.
    pub seq: u64,
    /// The event sent by the client.
    pub event: ExamEvent,
    /// The original server receipt time in UTC.
    pub received_at: OffsetDateTime,
}

/// The origin of a persisted incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IncidentSource {
    /// Detected by the server.
    Server,
    /// Reported by the client.
    Client,
}

/// An independent incident in the exam evidence history.
#[derive(Debug)]
pub struct IncidentRecord {
    /// The incident's unique ID.
    pub id: Uuid,
    /// The affected exam.
    pub exam_id: Uuid,
    /// The affected Accoding user.
    pub oj_user_id: i64,
    /// Where the incident was observed.
    pub source: IncidentSource,
    /// A stable incident category.
    pub kind: String,
    /// The recorded UTC occurrence time.
    pub occurred_at: OffsetDateTime,
    /// Additional incident detail, if available.
    pub detail: Option<String>,
}

/// Manages exams, enrollments and evidence in the server's database.
#[derive(Clone)]
pub struct AdminService(pub(crate) Service);

impl AdminService {
    /// Opens the database and applies migrations.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-SQLite URL or failed database initialization.
    pub async fn open(database_url: &str) -> Result<Self, DomainError> {
        Ok(Self(
            Service::open(database_url, OffsetDateTime::now_utc).await?,
        ))
    }

    /// Executes a management operation, recording the actor on a committed write.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid arguments, a missing resource, a conflicting
    /// write, or a failed database operation.
    pub async fn execute(
        &self,
        actor: String,
        op: AdminOperation,
    ) -> Result<AdminOutput, DomainError> {
        let now = self.0.now();
        if matches!(
            op,
            AdminOperation::ExamsCreate { .. }
                | AdminOperation::ParticipantsAdd { .. }
                | AdminOperation::ParticipantsRemove { .. }
                | AdminOperation::ParticipantsImport { .. }
        ) {
            db::write(&self.0.database, |tx| {
                let actor = actor.clone();
                let op = op.clone();
                Box::pin(async move { mutate(tx, &actor, &op, now).await })
            })
            .await
        } else {
            db::transaction(&self.0.database, |tx| Box::pin(query(tx, op, now))).await
        }
    }
}

async fn mutate(
    tx: &DatabaseTransaction,
    actor: &str,
    op: &AdminOperation,
    now: OffsetDateTime,
) -> Result<AdminOutput, DomainError> {
    match *op {
        AdminOperation::ExamsCreate {
            ref title,
            start_at,
            end_at,
            policy,
        } => {
            if start_at >= end_at
                || RecordingPolicy::new(policy.segment_duration_secs, policy.fps).is_err()
            {
                return Err(DomainError::InvalidInput);
            }
            let exam_id = Uuid::new_v4();
            let id = exam_id.to_string();
            exam::Entity::insert(exam::ActiveModel {
                exam_id: Set(id.clone()),
                title: Set(title.clone()),
                start_at: Set(db::input_time(start_at)?),
                end_at: Set(db::input_time(end_at)?),
                segment_duration_secs: Set(i64::from(policy.segment_duration_secs)),
                fps: Set(i64::from(policy.fps)),
            })
            .exec_without_returning(tx)
            .await?;
            audit(tx, actor, &id, "exams_create", None, 1, now).await?;
            Ok(AdminOutput::ExamCreated { exam_id })
        }
        AdminOperation::ParticipantsAdd {
            exam_id,
            oj_user_id,
        } => {
            let key = StudentKey::new(exam_id, oj_user_id)?;
            db::exam(tx, &key.exam).await?;
            if add_participant(tx, &key.exam, key.user).await? == 0 {
                return Err(DomainError::Conflict);
            }
            audit(
                tx,
                actor,
                &key.exam,
                "participants_add",
                Some(&[key.user]),
                1,
                now,
            )
            .await?;
            Ok(AdminOutput::ParticipantAdded)
        }
        AdminOperation::ParticipantsRemove {
            exam_id,
            oj_user_id,
        } => {
            let key = StudentKey::new(exam_id, oj_user_id)?;
            db::exam(tx, &key.exam).await?;
            let deleted = enrollment::Entity::delete_by_id(key.id()).exec(tx).await?;
            if deleted.rows_affected == 0 {
                return Err(DomainError::NotFound);
            }
            audit(
                tx,
                actor,
                &key.exam,
                "participants_remove",
                Some(&[key.user]),
                1,
                now,
            )
            .await?;
            Ok(AdminOutput::ParticipantRemoved)
        }
        AdminOperation::ParticipantsImport {
            exam_id,
            ref oj_user_ids,
        } => import_participants(tx, actor, exam_id, oj_user_ids, now).await,
        _ => Err(DomainError::InvalidInput),
    }
}

async fn import_participants(
    tx: &DatabaseTransaction,
    actor: &str,
    exam_id: Uuid,
    oj_user_ids: &[i64],
    now: OffsetDateTime,
) -> Result<AdminOutput, DomainError> {
    for &id in oj_user_ids {
        db::validate_user(id)?;
    }
    let users: BTreeSet<_> = oj_user_ids.iter().copied().collect();
    let users: Vec<_> = users.into_iter().collect();
    let exam = exam_id.to_string();
    db::exam(tx, &exam).await?;
    let mut count = 0;
    for &user in &users {
        count += add_participant(tx, &exam, user).await?;
    }
    audit(
        tx,
        actor,
        &exam,
        "participants_import",
        Some(&users),
        count,
        now,
    )
    .await?;
    Ok(AdminOutput::ParticipantsImported { count })
}

async fn add_participant(
    tx: &DatabaseTransaction,
    exam: &str,
    user: i64,
) -> Result<usize, DomainError> {
    let inserted = enrollment::Entity::insert(enrollment::ActiveModel {
        exam_id: Set(exam.into()),
        oj_user_id: Set(user),
    })
    .on_conflict_do_nothing()
    .exec_without_returning(tx)
    .await?;
    Ok(usize::from(matches!(
        inserted,
        sea_orm::TryInsertResult::Inserted(1)
    )))
}

async fn audit(
    tx: &DatabaseTransaction,
    actor: &str,
    exam: &str,
    operation: &str,
    target_users: Option<&[i64]>,
    count: usize,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    stored_audit::Entity::insert(stored_audit::ActiveModel {
        audit_id: Set(Uuid::new_v4().to_string()),
        actor: Set(actor.into()),
        operation: Set(operation.into()),
        exam_id: Set(exam.into()),
        target_users: Set(target_users.map(|users| db::encode(&users)).transpose()?),
        change_count: Set(i64::try_from(count).map_err(DomainError::storage)?),
        occurred_at: Set(db::stored_time(now)?),
    })
    .exec_without_returning(tx)
    .await?;
    Ok(())
}

async fn query(
    tx: &DatabaseTransaction,
    op: AdminOperation,
    now: OffsetDateTime,
) -> Result<AdminOutput, DomainError> {
    match op {
        AdminOperation::ExamsList => Ok(AdminOutput::Exams(db::exams(tx, None, now).await?)),
        AdminOperation::ExamsShow { exam_id } => {
            let exam = db::exam(tx, &exam_id.to_string()).await?;
            let policy = exam.policy()?;
            Ok(AdminOutput::ExamDetail {
                summary: exam.summary(now)?,
                policy,
            })
        }
        AdminOperation::ParticipantsList { exam_id } => participants(tx, exam_id).await,
        AdminOperation::StudentsStatus { exam_id } => students(tx, exam_id).await,
        AdminOperation::StudentsShow {
            exam_id,
            oj_user_id,
        } => {
            let key = StudentKey::new(exam_id, oj_user_id)?;
            let runtime = Runtime::get(tx, &key).await?;
            Ok(AdminOutput::Student(runtime.try_into()?))
        }
        AdminOperation::EventsList {
            exam_id,
            oj_user_id,
        } => {
            let key = StudentKey::new(exam_id, oj_user_id)?;
            events(tx, &key).await
        }
        AdminOperation::IncidentsList {
            exam_id,
            oj_user_id,
            kind,
            since,
            until,
        } => {
            IncidentFilter {
                exam_id,
                oj_user_id,
                kind,
                since,
                until,
            }
            .list(tx)
            .await
        }
        AdminOperation::IncidentsShow { incident_id } => {
            let incident = incident::Entity::find()
                .filter(incident::Column::IncidentId.eq(incident_id.to_string()))
                .one(tx)
                .await?
                .ok_or(DomainError::NotFound)?;
            Ok(AdminOutput::Incident(incident.try_into()?))
        }
        AdminOperation::RecordingsShow {
            exam_id,
            oj_user_id,
        } => {
            let key = StudentKey::new(exam_id, oj_user_id)?;
            recording(tx, &key).await
        }
        _ => Err(DomainError::InvalidInput),
    }
}

async fn participants(tx: &DatabaseTransaction, exam_id: Uuid) -> Result<AdminOutput, DomainError> {
    let id = exam_id.to_string();
    db::exam(tx, &id).await?;
    let participants = enrollment::Entity::find()
        .filter(enrollment::Column::ExamId.eq(id))
        .order_by_asc(enrollment::Column::OjUserId)
        .all(tx)
        .await?
        .into_iter()
        .map(|p| ParticipantRecord {
            exam_id,
            oj_user_id: p.oj_user_id,
        })
        .collect();
    Ok(AdminOutput::Participants(participants))
}

async fn students(tx: &DatabaseTransaction, exam_id: Uuid) -> Result<AdminOutput, DomainError> {
    let id = exam_id.to_string();
    db::exam(tx, &id).await?;
    let students = runtime::Entity::find()
        .filter(runtime::Column::ExamId.eq(id))
        .order_by_asc(runtime::Column::OjUserId)
        .all(tx)
        .await?
        .into_iter()
        .map(StudentRuntime::try_from)
        .collect::<Result<_, _>>()?;
    Ok(AdminOutput::Students(students))
}

impl TryFrom<Runtime> for StudentRuntime {
    type Error = DomainError;

    fn try_from(value: Runtime) -> Result<Self, Self::Error> {
        Ok(Self {
            exam_id: db::parse_uuid(&value.exam_id)?,
            oj_user_id: value.oj_user_id,
            connected: value.connected,
            execution: db::parse_enum(&value.execution)?,
            last_seen_at: Some(db::parse_time(&value.last_seen_at)?),
        })
    }
}

async fn events(tx: &DatabaseTransaction, key: &StudentKey) -> Result<AdminOutput, DomainError> {
    Runtime::get(tx, key).await?;
    let events = event::Entity::find()
        .filter(event::Column::ExamId.eq(key.exam.as_str()))
        .filter(event::Column::OjUserId.eq(key.user))
        .order_by_asc(event::Column::Seq)
        .all(tx)
        .await?
        .into_iter()
        .map(|row| -> Result<_, DomainError> {
            Ok(EventRecord {
                exam_id: key.exam_id(),
                oj_user_id: key.user,
                seq: db::decode_seq(&row.seq)?,
                event: db::decode(&row.event)?,
                received_at: db::parse_time(&row.received_at)?,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(AdminOutput::Events(events))
}

impl TryFrom<incident::Model> for IncidentRecord {
    type Error = DomainError;

    fn try_from(value: incident::Model) -> Result<Self, Self::Error> {
        Ok(Self {
            id: db::parse_uuid(&value.incident_id)?,
            exam_id: db::parse_uuid(&value.exam_id)?,
            oj_user_id: value.oj_user_id,
            source: db::parse_enum(&value.source)?,
            kind: value.kind,
            occurred_at: db::parse_time(&value.occurred_at)?,
            detail: value.detail,
        })
    }
}

struct IncidentFilter {
    exam_id: Uuid,
    oj_user_id: Option<i64>,
    kind: Option<String>,
    since: Option<OffsetDateTime>,
    until: Option<OffsetDateTime>,
}

impl IncidentFilter {
    async fn list(self, tx: &DatabaseTransaction) -> Result<AdminOutput, DomainError> {
        use incident::Column;

        let exam_id = self.exam_id.to_string();
        if let Some(user) = self.oj_user_id {
            db::validate_user(user)?;
        }
        if self
            .since
            .zip(self.until)
            .is_some_and(|(since, until)| since > until)
        {
            return Err(DomainError::InvalidInput);
        }
        let since = self.since.map(db::input_time).transpose()?;
        let until = self.until.map(db::input_time).transpose()?;
        db::exam(tx, &exam_id).await?;

        let incidents = incident::Entity::find()
            .filter(Column::ExamId.eq(exam_id))
            .apply_if(self.oj_user_id, |query, user| {
                query.filter(Column::OjUserId.eq(user))
            })
            .apply_if(self.kind, |query, kind| query.filter(Column::Kind.eq(kind)))
            .apply_if(since, |query, since| {
                query.filter(Column::OccurredAt.gte(since))
            })
            .apply_if(until, |query, until| {
                query.filter(Column::OccurredAt.lte(until))
            })
            .order_by_asc(Column::OccurredAt)
            .order_by_asc(Column::Ordinal)
            .all(tx)
            .await?
            .into_iter()
            .map(IncidentRecord::try_from)
            .collect::<Result<_, _>>()?;
        Ok(AdminOutput::Incidents(incidents))
    }
}

async fn recording(tx: &DatabaseTransaction, key: &StudentKey) -> Result<AdminOutput, DomainError> {
    let runtime = Runtime::get(tx, key).await?;
    let state = runtime.state()?;
    if matches!(
        state.execution,
        ExecutionState::Finished | ExecutionState::Finalizing
    ) && state.recording == RecordingState::Failed
    {
        return Err(DomainError::NotFound);
    }
    let manifest = manifest::Entity::find_by_id(key.id())
        .one(tx)
        .await?
        .ok_or(DomainError::NotFound)?;
    Ok(AdminOutput::Recording(db::decode(&manifest.manifest)?))
}
