use std::{future::Future, pin::Pin, time::Duration};

use cvbbmso_protocol::{ClientState, ExamPhase, ExamSummary, RecordingPolicy};
use sea_orm::sqlx::ConnectOptions as _;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, DbErr, FromQueryResult,
    RuntimeErr, Statement, TransactionTrait, sqlx,
};
use sea_orm_migration::MigratorTrait;
use serde::{Serialize, de::DeserializeOwned};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{DomainError, migration::Migrator};

macro_rules! sql {
    ($query:expr $(, $arg:expr)* $(,)?) => {
        sea_orm::Statement::from_sql_and_values(
            sea_orm::DbBackend::Sqlite,
            $query,
            [$(sea_orm::Value::from($arg)),*],
        )
    };
}
pub(crate) use sql;

pub(crate) async fn open(url: &str) -> Result<DatabaseConnection, DomainError> {
    if !url.starts_with("sqlite:") {
        return Err(DomainError::InvalidInput);
    }
    let options = url
        .parse::<sqlx::sqlite::SqliteConnectOptions>()
        .map_err(DomainError::storage)?
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(250))
        .disable_statement_logging();
    // A memory database shares the lifetime of its single pooled connection.
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options)
        .await
        .map_err(DomainError::storage)?;
    let db = DatabaseConnection::from(pool);
    write(&db, |transaction| {
        Box::pin(async move {
            Migrator::up(transaction, None).await?;
            Ok(())
        })
    })
    .await?;
    Ok(db)
}

type TransactionFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>>;

pub(crate) async fn write<T>(
    db: &DatabaseConnection,
    mut operation: impl for<'a> FnMut(&'a DatabaseTransaction) -> TransactionFuture<'a, T>,
) -> Result<T, DomainError> {
    // A competing CLI or server writer can invalidate a deferred transaction's read snapshot.
    // Retry the whole operation so authorization and state are read again.
    const ATTEMPTS: usize = 4;
    for _ in 1..ATTEMPTS {
        match transaction(db, &mut operation).await {
            Err(error) if busy(&error) => tokio::task::yield_now().await,
            result => return result,
        }
    }
    transaction(db, operation).await
}

pub(crate) async fn transaction<T>(
    db: &DatabaseConnection,
    operation: impl for<'a> FnOnce(&'a DatabaseTransaction) -> TransactionFuture<'a, T>,
) -> Result<T, DomainError> {
    let tx = db.begin().await?;
    match operation(&tx).await {
        Ok(output) => {
            tx.commit().await?;
            Ok(output)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

fn busy(error: &DomainError) -> bool {
    let DomainError::Storage { source } = error else {
        return false;
    };
    let Some(
        DbErr::Exec(RuntimeErr::SqlxError(error))
        | DbErr::Query(RuntimeErr::SqlxError(error))
        | DbErr::Conn(RuntimeErr::SqlxError(error)),
    ) = source.downcast_ref::<DbErr>()
    else {
        return false;
    };
    let sqlx::Error::Database(source) = error.as_ref() else {
        return false;
    };
    source
        .code()
        .and_then(|code| code.parse::<u32>().ok())
        .is_some_and(|code| matches!(code & 0xff, 5 | 6))
}

pub(crate) fn timestamp(value: OffsetDateTime) -> Result<String, time::error::Format> {
    let value = value
        .checked_to_offset(UtcOffset::UTC)
        .ok_or(time::error::Format::InvalidComponent("year"))?;
    let rfc3339 = value.format(&Rfc3339)?;
    let whole_seconds = rfc3339
        .get(..19)
        .ok_or(time::error::Format::InvalidComponent("year"))?;
    Ok(format!("{whole_seconds}.{:09}Z", value.nanosecond()))
}

pub(crate) fn stored_time(value: OffsetDateTime) -> Result<String, DomainError> {
    timestamp(value).map_err(DomainError::storage)
}

pub(crate) fn input_time(value: OffsetDateTime) -> Result<String, DomainError> {
    timestamp(value).map_err(|_| DomainError::InvalidInput)
}

pub(crate) fn parse_time(value: &str) -> Result<OffsetDateTime, DomainError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(DomainError::storage)
}

pub(crate) fn parse_uuid(value: &str) -> Result<Uuid, DomainError> {
    Uuid::parse_str(value).map_err(DomainError::storage)
}

pub(crate) fn encode(value: &impl Serialize) -> Result<String, DomainError> {
    serde_json::to_string(value).map_err(DomainError::storage)
}

pub(crate) fn decode<T: DeserializeOwned>(value: &str) -> Result<T, DomainError> {
    serde_json::from_str(value).map_err(DomainError::storage)
}

pub(crate) fn encode_seq(value: u64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

pub(crate) fn decode_seq(value: &[u8]) -> Result<u64, DomainError> {
    value
        .try_into()
        .map(u64::from_be_bytes)
        .map_err(DomainError::storage)
}

#[derive(Clone, Debug)]
pub(crate) struct StudentKey {
    exam_id: Uuid,
    pub(crate) exam: String,
    pub(crate) user: i64,
}

impl StudentKey {
    pub(crate) fn new(exam_id: Uuid, user: i64) -> Result<Self, DomainError> {
        validate_user(user)?;
        Ok(Self {
            exam_id,
            exam: exam_id.to_string(),
            user,
        })
    }

    pub(crate) fn exam_id(&self) -> Uuid {
        self.exam_id
    }
}

pub(crate) fn validate_user(id: i64) -> Result<(), DomainError> {
    if id <= 0 {
        return Err(DomainError::InvalidInput);
    }
    Ok(())
}

#[derive(FromQueryResult)]
pub(crate) struct StoredExam {
    exam_id: String,
    title: String,
    start_at: String,
    end_at: String,
    segment_duration_secs: i64,
    fps: i64,
}

impl StoredExam {
    pub(crate) fn times(&self) -> Result<(OffsetDateTime, OffsetDateTime), DomainError> {
        Ok((parse_time(&self.start_at)?, parse_time(&self.end_at)?))
    }

    pub(crate) fn policy(&self) -> Result<RecordingPolicy, DomainError> {
        let segment_duration_secs =
            u32::try_from(self.segment_duration_secs).map_err(DomainError::storage)?;
        let fps = u32::try_from(self.fps).map_err(DomainError::storage)?;
        RecordingPolicy::new(segment_duration_secs, fps).map_err(DomainError::storage)
    }

    pub(crate) fn summary(self, now: OffsetDateTime) -> Result<ExamSummary, DomainError> {
        let (start_at, end_at) = self.times()?;
        ExamSummary::new(
            parse_uuid(&self.exam_id)?,
            self.title,
            start_at,
            end_at,
            phase(start_at, end_at, now),
        )
        .map_err(DomainError::storage)
    }
}

pub(crate) async fn exam(db: &impl ConnectionTrait, exam: &str) -> Result<StoredExam, DomainError> {
    StoredExam::find_by_statement(sql!("SELECT * FROM exams WHERE exam_id = ?", exam))
        .one(db)
        .await?
        .ok_or(DomainError::NotFound)
}

pub(crate) async fn exams(
    db: &impl ConnectionTrait,
    user: Option<i64>,
    now: OffsetDateTime,
) -> Result<Vec<ExamSummary>, DomainError> {
    let statement = match user {
        Some(user) => sql!(
            "SELECT e.* FROM enrollments AS p JOIN exams AS e USING (exam_id)
             WHERE p.oj_user_id = ? ORDER BY e.start_at, e.exam_id",
            user,
        ),
        None => Statement::from_string(
            DbBackend::Sqlite,
            "SELECT * FROM exams ORDER BY start_at, exam_id",
        ),
    };
    StoredExam::find_by_statement(statement)
        .all(db)
        .await?
        .into_iter()
        .map(|exam| exam.summary(now))
        .collect()
}

pub(crate) async fn enrollment(
    db: &impl ConnectionTrait,
    key: &StudentKey,
) -> Result<StoredExam, DomainError> {
    let exam = exam(db, &key.exam).await?;
    if db
        .query_one_raw(sql!(
            "SELECT 1 FROM enrollments WHERE exam_id = ? AND oj_user_id = ?",
            key.exam.as_str(),
            key.user,
        ))
        .await?
        .is_none()
    {
        return Err(DomainError::NotEnrolled);
    }
    Ok(exam)
}

pub(crate) fn phase(
    start_at: OffsetDateTime,
    end_at: OffsetDateTime,
    now: OffsetDateTime,
) -> ExamPhase {
    if now < start_at {
        ExamPhase::Scheduled
    } else if now < end_at {
        ExamPhase::Running
    } else {
        ExamPhase::Ended
    }
}

#[derive(FromQueryResult)]
pub(crate) struct Runtime {
    pub(crate) exam_id: String,
    pub(crate) oj_user_id: i64,
    pub(crate) instance_id: String,
    pub(crate) connection_id: String,
    pub(crate) connected: bool,
    pub(crate) execution: String,
    pub(crate) recording: String,
    pub(crate) last_seen_at: String,
    pub(crate) ack_event_through: Vec<u8>,
    pub(crate) lost_incident_id: Option<String>,
}

impl Runtime {
    pub(crate) async fn find(
        db: &impl ConnectionTrait,
        key: &StudentKey,
    ) -> Result<Option<Self>, DomainError> {
        Ok(Self::find_by_statement(sql!(
            "SELECT * FROM student_runtimes WHERE exam_id = ? AND oj_user_id = ?",
            key.exam.as_str(),
            key.user,
        ))
        .one(db)
        .await?)
    }

    pub(crate) async fn get(
        db: &impl ConnectionTrait,
        key: &StudentKey,
    ) -> Result<Self, DomainError> {
        Self::find(db, key).await?.ok_or(DomainError::NotFound)
    }

    pub(crate) async fn online(db: &impl ConnectionTrait) -> Result<Vec<Self>, DomainError> {
        Ok(Self::find_by_statement(sql!(
            "SELECT * FROM student_runtimes WHERE connected = 1 ORDER BY exam_id, oj_user_id"
        ))
        .all(db)
        .await?)
    }

    pub(crate) fn key(&self) -> Result<StudentKey, DomainError> {
        StudentKey::new(parse_uuid(&self.exam_id)?, self.oj_user_id)
    }

    pub(crate) fn state(&self) -> Result<ClientState, DomainError> {
        Ok(ClientState {
            execution: parse_enum(&self.execution)?,
            recording: parse_enum(&self.recording)?,
        })
    }
}

pub(crate) fn enum_name(value: impl Serialize) -> Result<String, DomainError> {
    match serde_json::to_value(value).map_err(DomainError::storage)? {
        serde_json::Value::String(value) => Ok(value),
        _ => Err(DomainError::storage(DbErr::Type(
            "expected enum name".into(),
        ))),
    }
}

pub(crate) fn parse_enum<T: DeserializeOwned>(value: &str) -> Result<T, DomainError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(DomainError::storage)
}
