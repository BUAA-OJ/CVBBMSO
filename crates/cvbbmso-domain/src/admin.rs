use std::collections::BTreeSet;

use cvbbmso_protocol::{
    ExamEvent, ExamSummary, ExecutionState, RecordingManifest, RecordingPolicy, RecordingState,
};
use sea_orm::{ConnectionTrait, DatabaseTransaction, DbBackend, FromQueryResult, Statement, Value};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    DomainError, Service,
    db::{self, Runtime, StudentKey, sql},
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
            tx.execute_raw(sql!(
                "INSERT INTO exams (exam_id, title, start_at, end_at, segment_duration_secs, fps)
                 VALUES (?, ?, ?, ?, ?, ?)",
                id.as_str(),
                title.as_str(),
                db::input_time(start_at)?,
                db::input_time(end_at)?,
                i64::from(policy.segment_duration_secs),
                i64::from(policy.fps),
            ))
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
            let deleted = tx
                .execute_raw(sql!(
                    "DELETE FROM enrollments WHERE exam_id = ? AND oj_user_id = ?",
                    key.exam.as_str(),
                    key.user,
                ))
                .await?;
            if deleted.rows_affected() == 0 {
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
    let execution = tx
        .execute_raw(sql!(
            "INSERT INTO enrollments (exam_id, oj_user_id) VALUES (?, ?)
         ON CONFLICT (exam_id, oj_user_id) DO NOTHING",
            exam,
            user,
        ))
        .await?;
    usize::try_from(execution.rows_affected()).map_err(DomainError::storage)
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
    let target_users = target_users.map(|users| db::encode(&users)).transpose()?;
    tx.execute_raw(sql!(
        "INSERT INTO admin_audit (audit_id, actor, operation, exam_id, target_users, change_count, occurred_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        Uuid::new_v4().to_string(), actor, operation, exam, target_users,
        i64::try_from(count).map_err(DomainError::storage)?, db::stored_time(now)?,
    )).await?;
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
            let incident = StoredIncident::find_by_statement(sql!(
                "SELECT * FROM incidents WHERE incident_id = ?",
                incident_id.to_string(),
            ))
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

#[derive(FromQueryResult)]
struct Participant {
    oj_user_id: i64,
}

async fn participants(tx: &DatabaseTransaction, exam_id: Uuid) -> Result<AdminOutput, DomainError> {
    let id = exam_id.to_string();
    db::exam(tx, &id).await?;
    let participants = Participant::find_by_statement(sql!(
        "SELECT oj_user_id FROM enrollments WHERE exam_id = ? ORDER BY oj_user_id",
        id,
    ))
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
    let students = Runtime::find_by_statement(sql!(
        "SELECT * FROM student_runtimes WHERE exam_id = ? ORDER BY oj_user_id",
        id,
    ))
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

#[derive(FromQueryResult)]
struct StoredEvent {
    seq: Vec<u8>,
    event: String,
    received_at: String,
}

async fn events(tx: &DatabaseTransaction, key: &StudentKey) -> Result<AdminOutput, DomainError> {
    Runtime::get(tx, key).await?;
    let events = StoredEvent::find_by_statement(sql!(
        "SELECT seq, event, received_at FROM events WHERE exam_id = ? AND oj_user_id = ? ORDER BY seq",
        key.exam.as_str(), key.user,
    ))
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

#[derive(FromQueryResult)]
struct StoredIncident {
    incident_id: String,
    exam_id: String,
    oj_user_id: i64,
    source: String,
    kind: String,
    occurred_at: String,
    detail: Option<String>,
}

impl TryFrom<StoredIncident> for IncidentRecord {
    type Error = DomainError;

    fn try_from(value: StoredIncident) -> Result<Self, Self::Error> {
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

        let mut query = String::from("SELECT * FROM incidents WHERE exam_id = ?");
        let mut args: Vec<Value> = vec![exam_id.into()];
        for (fragment, value) in [
            (" AND oj_user_id = ?", self.oj_user_id.map(Value::from)),
            (" AND kind = ?", self.kind.map(Value::from)),
            (" AND occurred_at >= ?", since.map(Value::from)),
            (" AND occurred_at <= ?", until.map(Value::from)),
        ] {
            if let Some(value) = value {
                query.push_str(fragment);
                args.push(value);
            }
        }
        query.push_str(" ORDER BY occurred_at, ordinal");
        let incidents = StoredIncident::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            query,
            args,
        ))
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
    let manifest = tx
        .query_one_raw(sql!(
            "SELECT manifest FROM recording_manifests WHERE exam_id = ? AND oj_user_id = ?",
            key.exam.as_str(),
            key.user,
        ))
        .await?
        .ok_or(DomainError::NotFound)?;
    let json: String = manifest.try_get("", "manifest")?;
    Ok(AdminOutput::Recording(db::decode(&json)?))
}
