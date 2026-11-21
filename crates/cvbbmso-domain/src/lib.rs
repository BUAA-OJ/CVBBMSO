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
pub use exam::{ConnectOutcome, DirectedCommand, DisconnectReason, ExamService};

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

    fn storage(source: impl Error + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

impl From<DbErr> for DomainError {
    fn from(error: DbErr) -> Self {
        Self::storage(error)
    }
}

#[derive(Clone)]
struct Service {
    database: DatabaseConnection,
    clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
}

impl Service {
    async fn open(
        database_url: &str,
        clock: impl Fn() -> OffsetDateTime + Send + Sync + 'static,
    ) -> Result<Self, DomainError> {
        let database = db::open(database_url).await?;
        Ok(Self {
            database,
            clock: Arc::new(clock),
        })
    }

    fn now(&self) -> OffsetDateTime {
        (self.clock)().to_offset(UtcOffset::UTC)
    }
}

#[cfg(test)]
mod tests;
