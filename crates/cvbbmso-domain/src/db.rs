use std::{future::Future, pin::Pin, time::Duration};

use cvbbmso_protocol::{ClientState, ExamPhase, ExamSummary, RecordingPolicy};
use migration::{Migrator, MigratorTrait};
use sea_orm::{
    ColumnTrait, ConnectOptions, ConnectionTrait, Database, DatabaseConnection,
    DatabaseTransaction, DbErr, EntityTrait, JoinType, QueryFilter, QueryOrder, QuerySelect,
    QueryTrait, RelationTrait, RuntimeErr, TransactionTrait, sqlx,
};
use serde::{Serialize, de::DeserializeOwned};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    DomainError,
    entity::{enrollment, exam as exam_table, runtime},
};

pub(crate) use crate::entity::{exam::Model as StoredExam, runtime::Model as Runtime};

pub(crate) async fn open(url: &str) -> Result<DatabaseConnection, DomainError> {
    if !url.starts_with("sqlite:") {
        return Err(DomainError::InvalidInput);
    }
    let mut options = ConnectOptions::new(url);
    // A memory database shares the lifetime of its single pooled connection.
    options
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .sqlx_logging(false)
        .map_sqlx_sqlite_opts(|options| {
            options
                .foreign_keys(true)
                .busy_timeout(Duration::from_millis(250))
        });
    let db = Database::connect(options).await?;
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

pub(crate) fn busy(error: &DomainError) -> bool {
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

    pub(crate) fn id(&self) -> (String, i64) {
        (self.exam.clone(), self.user)
    }
}

pub(crate) fn validate_user(id: i64) -> Result<(), DomainError> {
    if id <= 0 {
        return Err(DomainError::InvalidInput);
    }
    Ok(())
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
    exam_table::Entity::find_by_id(exam)
        .one(db)
        .await?
        .ok_or(DomainError::NotFound)
}

pub(crate) async fn exams(
    db: &impl ConnectionTrait,
    user: Option<i64>,
    now: OffsetDateTime,
) -> Result<Vec<ExamSummary>, DomainError> {
    exam_table::Entity::find()
        .apply_if(user, |query, user| {
            query
                .join(JoinType::InnerJoin, enrollment::Relation::Exam.def().rev())
                .filter(enrollment::Column::OjUserId.eq(user))
        })
        .order_by_asc(exam_table::Column::StartAt)
        .order_by_asc(exam_table::Column::ExamId)
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
    require_enrollment(db, key).await?;
    Ok(exam)
}

pub(crate) async fn require_enrollment(
    db: &impl ConnectionTrait,
    key: &StudentKey,
) -> Result<(), DomainError> {
    if enrollment::Entity::find_by_id(key.id())
        .one(db)
        .await?
        .is_none()
    {
        return Err(DomainError::NotEnrolled);
    }
    Ok(())
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

impl Runtime {
    pub(crate) async fn find(
        db: &impl ConnectionTrait,
        key: &StudentKey,
    ) -> Result<Option<Self>, DomainError> {
        Ok(runtime::Entity::find_by_id(key.id()).one(db).await?)
    }

    pub(crate) async fn get(
        db: &impl ConnectionTrait,
        key: &StudentKey,
    ) -> Result<Self, DomainError> {
        Self::find(db, key).await?.ok_or(DomainError::NotFound)
    }

    pub(crate) async fn connected(
        db: &impl ConnectionTrait,
        key: &StudentKey,
    ) -> Result<Self, DomainError> {
        require_enrollment(db, key).await?;
        let runtime = Self::get(db, key).await?;
        if !runtime.connected {
            return Err(DomainError::NotEnrolled);
        }
        Ok(runtime)
    }

    pub(crate) async fn online(db: &impl ConnectionTrait) -> Result<Vec<Self>, DomainError> {
        Ok(runtime::Entity::find()
            .filter(runtime::Column::Connected.eq(true))
            .order_by_asc(runtime::Column::ExamId)
            .order_by_asc(runtime::Column::OjUserId)
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
