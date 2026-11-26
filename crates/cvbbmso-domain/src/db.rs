use std::{borrow::Cow, error::Error, future::Future, pin::Pin, time::Duration};

use cvbbmso_protocol::{ClientState, ExamPhase, ExamSummary, RecordingPolicy};
use migration::{Migrator, MigratorTrait};
use sea_orm::{
    ColumnTrait, ConnectOptions, ConnectionTrait, Database, DatabaseConnection,
    DatabaseTransaction, DbErr, EntityTrait, JoinType, QueryFilter, QueryOrder, QuerySelect,
    QueryTrait, RelationTrait, RuntimeErr, SqliteTransactionMode, TransactionOptions,
    TransactionTrait,
    sqlx::{
        self,
        sqlite::{SqliteJournalMode, SqliteSynchronous},
    },
};
use serde::{Serialize, de::DeserializeOwned};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    DomainError,
    entity::{enrollment, exam as exam_table, runtime},
};

pub(crate) use crate::entity::{exam::Model as StoredExam, runtime::Model as Runtime};

const READ_POOL_MAX_CONNECTIONS: u32 = 4;
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

pub(crate) struct Pools {
    pub(crate) writer: DatabaseConnection,
    pub(crate) reader: DatabaseConnection,
}

fn is_memory_url(url: &str) -> bool {
    let without_scheme = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"));
    let Some(rest) = without_scheme else {
        return false;
    };
    let path = rest.split('?').next().unwrap_or("");
    let query = rest.split_once('?').map_or("", |(_, q)| q);
    path == ":memory:"
        || query
            .split('&')
            .any(|param| param.eq_ignore_ascii_case("mode=memory"))
}

pub(crate) async fn open(url: &str) -> Result<Pools, DomainError> {
    if !url.starts_with("sqlite:") {
        return Err(DomainError::InvalidInput);
    }
    let memory = is_memory_url(url);
    let mut write_options = ConnectOptions::new(url);
    // Keep a single writer connection; memory databases also share this connection for reads.
    write_options
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .sqlx_logging(false)
        .map_sqlx_sqlite_opts(move |options| {
            let options = options
                .foreign_keys(true)
                .busy_timeout(BUSY_TIMEOUT)
                .pragma("cache_size", "-32768");
            if memory {
                options
            } else {
                options
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Full)
                    .pragma("wal_autocheckpoint", "1000")
            }
        });
    let writer = Database::connect(write_options).await?;
    write(&writer, |transaction| {
        Box::pin(async move {
            Migrator::up(transaction, None).await?;
            Ok(())
        })
    })
    .await?;

    let reader = if memory {
        writer.clone()
    } else {
        let mut read_options = ConnectOptions::new(url);
        read_options
            .max_connections(READ_POOL_MAX_CONNECTIONS)
            .min_connections(1)
            .sqlx_logging(false)
            .map_sqlx_sqlite_opts(move |options| {
                options
                    .read_only(true)
                    .create_if_missing(false)
                    .foreign_keys(true)
                    .busy_timeout(BUSY_TIMEOUT)
                    .pragma("cache_size", "-32768")
            });
        Database::connect(read_options).await?
    };

    Ok(Pools { writer, reader })
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

pub(crate) async fn immediate_transaction<T>(
    db: &DatabaseConnection,
    operation: impl for<'a> FnOnce(&'a DatabaseTransaction) -> TransactionFuture<'a, T>,
) -> Result<T, DomainError> {
    let tx = db
        .begin_with_options(TransactionOptions {
            sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
            ..Default::default()
        })
        .await?;
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
    if let DomainError::Storage { source } = error
        && let Some(shared) = source.downcast_ref::<crate::SharedStorageError>()
    {
        return busy(&shared.0);
    }
    sqlite_error_code(error).is_some_and(|code| matches!(code & 0xff, 5 | 6))
}

pub(crate) fn storage_fault(error: &DomainError) -> bool {
    let DomainError::Storage { source } = error else {
        return false;
    };
    if let Some(shared) = source.downcast_ref::<crate::SharedStorageError>() {
        return storage_fault(&shared.0);
    }
    let Some(db_err) = source.downcast_ref::<DbErr>() else {
        return false;
    };
    let (DbErr::Exec(RuntimeErr::SqlxError(err))
    | DbErr::Query(RuntimeErr::SqlxError(err))
    | DbErr::Conn(RuntimeErr::SqlxError(err))) = db_err
    else {
        return matches!(db_err, DbErr::ConnectionAcquire(_));
    };
    match err.as_ref() {
        sqlx::Error::Io(_) | sqlx::Error::PoolClosed => true,
        sqlx::Error::Database(source) => source
            .code()
            .and_then(|code| code.parse::<u32>().ok())
            .is_some_and(|code| matches!(code & 0xff, 8 | 10 | 11 | 13 | 14 | 26)),
        _ => false,
    }
}

fn sqlite_error_code(error: &DomainError) -> Option<u32> {
    let DomainError::Storage { source } = error else {
        return None;
    };
    let (DbErr::Exec(RuntimeErr::SqlxError(error))
    | DbErr::Query(RuntimeErr::SqlxError(error))
    | DbErr::Conn(RuntimeErr::SqlxError(error))) = source.downcast_ref::<DbErr>()?
    else {
        return None;
    };
    let sqlx::Error::Database(source) = error.as_ref() else {
        return None;
    };
    source.code()?.parse::<u32>().ok()
}

pub(crate) fn storage_diagnostic(source: &(dyn Error + 'static)) -> Cow<'static, str> {
    if let Some(shared) = source.downcast_ref::<crate::SharedStorageError>() {
        return shared.0.diagnostic();
    }
    // Driver and conversion messages can quote database paths or stored values.
    if let Some(error) = source.downcast_ref::<DbErr>() {
        let (operation, runtime) = match error {
            DbErr::Conn(runtime) => ("connection", runtime),
            DbErr::Exec(runtime) => ("execution", runtime),
            DbErr::Query(runtime) => ("query", runtime),
            DbErr::ConnectionAcquire(reason) => return reason.to_string().into(),
            DbErr::Migration(_) => return "database migration failed".into(),
            DbErr::Type(_) | DbErr::TryIntoErr { .. } => {
                return "database value type conversion failed".into();
            }
            _ => return "database operation failed".into(),
        };
        if let RuntimeErr::SqlxError(error) = runtime {
            return match error.as_ref() {
                sqlx::Error::Database(error) => {
                    let code = error.code().and_then(|code| code.parse::<u32>().ok());
                    match code {
                        Some(code) => format!("SQLite {operation} failed (code {code})").into(),
                        None => format!("SQLite {operation} failed").into(),
                    }
                }
                sqlx::Error::Io(error) => {
                    format!("database {operation} I/O failed ({:?})", error.kind()).into()
                }
                sqlx::Error::PoolTimedOut => "database pool timed out".into(),
                sqlx::Error::PoolClosed => "database pool is closed".into(),
                _ => format!("database driver {operation} failed").into(),
            };
        }
        return format!("database {operation} failed").into();
    }
    if let Some(error) = source.downcast_ref::<serde_json::Error>() {
        return format!("stored JSON error ({:?})", error.classify()).into();
    }
    if source.is::<uuid::Error>() {
        return "invalid stored UUID".into();
    }
    if source.is::<time::error::Parse>() || source.is::<time::error::Format>() {
        return "invalid stored timestamp".into();
    }
    "stored value conversion failed".into()
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

    pub(crate) async fn connected_generation(
        db: &impl ConnectionTrait,
        key: &StudentKey,
        connection_id: Option<Uuid>,
    ) -> Result<Self, DomainError> {
        require_enrollment(db, key).await?;
        let runtime = Self::get(db, key).await?;
        if !runtime.connected {
            return Err(DomainError::NotEnrolled);
        }
        if let Some(expected) = connection_id
            && runtime.connection_id.as_deref() != Some(&expected.to_string())
        {
            return Err(DomainError::Conflict);
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
