//! Exam scheduling, persistence, administration, and evidence collection.

#![forbid(unsafe_code)]
// These names are re-exported at the crate root.
#![allow(clippy::module_name_repetitions)]

mod admin;
mod commands;
mod db;
mod entity;
mod events;
mod exam;

pub use admin::{
    AdminOperation, AdminOutput, AdminService, EventRecord, IncidentRecord, IncidentSource,
    ParticipantRecord, StudentRuntime,
};
pub use exam::{
    BatchOutput, BatchWrite, ConnectOutcome, DirectedCommand, DisconnectReason, ExamService,
    HeartbeatUpdate, ScheduledCommand,
};

use std::{borrow::Cow, error::Error, sync::Arc};

use sea_orm::{DatabaseConnection, DbErr};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};

/// Errors returned by the exam and administration services.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DomainError {
    /// The requested resource is absent.
    #[error("resource not found")]
    NotFound,
    /// The exam exists, but the user has no enrollment.
    #[error("user is not enrolled")]
    NotEnrolled,
    /// An input violates a domain or protocol invariant.
    #[error("invalid input")]
    InvalidInput,
    /// The operation conflicts with persisted state.
    #[error("conflicting state")]
    Conflict,
    /// Storage access, encoding, or decoding failed.
    #[error("storage failure")]
    Storage {
        /// The original storage or conversion error.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
}

impl DomainError {
    /// Returns a log message with error categories and numeric diagnostics.
    /// The message omits data carried in the underlying error's text.
    #[must_use]
    pub fn diagnostic(&self) -> Cow<'static, str> {
        match self {
            Self::NotFound => "resource not found".into(),
            Self::NotEnrolled => "user is not enrolled".into(),
            Self::InvalidInput => "invalid input".into(),
            Self::Conflict => "conflicting state".into(),
            Self::Storage { source } => db::storage_diagnostic(source.as_ref()),
        }
    }

    /// Returns whether a temporary SQLite lock prevented the operation.
    #[must_use]
    pub fn is_storage_busy(&self) -> bool {
        db::busy(self)
    }

    /// Returns whether a persistent storage or I/O failure prevented the operation.
    #[must_use]
    pub fn is_storage_fault(&self) -> bool {
        db::storage_fault(self)
    }

    /// Clones a shared error reference, preserving business variants and wrapping storage causes.
    #[must_use]
    pub fn clone_shared(error: &Arc<Self>) -> Self {
        match error.as_ref() {
            Self::NotFound => Self::NotFound,
            Self::NotEnrolled => Self::NotEnrolled,
            Self::InvalidInput => Self::InvalidInput,
            Self::Conflict => Self::Conflict,
            Self::Storage { .. } => Self::Storage {
                source: Box::new(SharedStorageError(Arc::clone(error))),
            },
        }
    }

    fn storage(source: impl Error + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

#[derive(Debug, Error)]
#[error("shared storage failure")]
struct SharedStorageError(#[source] Arc<DomainError>);

impl From<DbErr> for DomainError {
    fn from(error: DbErr) -> Self {
        Self::storage(error)
    }
}

#[derive(Clone)]
struct Service {
    writer: DatabaseConnection,
    reader: DatabaseConnection,
    clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
}

impl Service {
    async fn open(
        database_url: &str,
        clock: impl Fn() -> OffsetDateTime + Send + Sync + 'static,
    ) -> Result<Self, DomainError> {
        let pools = db::open(database_url).await?;
        Ok(Self {
            writer: pools.writer,
            reader: pools.reader,
            clock: Arc::new(clock),
        })
    }

    fn now(&self) -> OffsetDateTime {
        (self.clock)().to_offset(UtcOffset::UTC)
    }
}

#[cfg(test)]
mod tests;
