//! Exams as presented to an authenticated student.

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

use crate::ProtocolError;

/// An authenticated Accoding user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawAccodingUser")]
pub struct AccodingUser {
    /// Positive Accoding user id; the `oj_user_id` of the protocol.
    pub id: i64,
    /// Login name returned by the student site.
    pub username: String,
    /// Display name returned by the student site.
    pub nickname: String,
}

impl AccodingUser {
    /// Creates a user, rejecting non-positive ids.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidUserId`] when `id` is not positive.
    pub fn new(id: i64, username: String, nickname: String) -> Result<Self, ProtocolError> {
        if id <= 0 {
            return Err(ProtocolError::InvalidUserId);
        }
        Ok(Self {
            id,
            username,
            nickname,
        })
    }
}

impl TryFrom<RawAccodingUser> for AccodingUser {
    type Error = ProtocolError;

    fn try_from(raw: RawAccodingUser) -> Result<Self, Self::Error> {
        Self::new(raw.id, raw.username, raw.nickname)
    }
}

#[derive(Deserialize)]
struct RawAccodingUser {
    id: i64,
    username: String,
    nickname: String,
}

/// A single exam a student is enrolled in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawExamSummary")]
pub struct ExamSummary {
    /// Exam identity, generated when the exam is created.
    pub id: Uuid,
    /// Human-readable exam title.
    pub title: String,
    /// Fixed start time, normalized to UTC.
    #[serde(with = "crate::utc::rfc3339")]
    pub start_at: OffsetDateTime,
    /// Fixed end time, normalized to UTC.
    #[serde(with = "crate::utc::rfc3339")]
    pub end_at: OffsetDateTime,
    /// Phase derived from the server clock.
    pub phase: ExamPhase,
}

impl ExamSummary {
    /// Creates a summary, requiring `start_at <= end_at`.
    ///
    /// Times are normalized to UTC.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidTimeRange`] when `start_at` is later
    /// than `end_at`.
    pub fn new(
        id: Uuid,
        title: String,
        start_at: OffsetDateTime,
        end_at: OffsetDateTime,
        phase: ExamPhase,
    ) -> Result<Self, ProtocolError> {
        let start_at = start_at.to_offset(UtcOffset::UTC);
        let end_at = end_at.to_offset(UtcOffset::UTC);
        if start_at > end_at {
            return Err(ProtocolError::InvalidTimeRange);
        }
        Ok(Self {
            id,
            title,
            start_at,
            end_at,
            phase,
        })
    }
}

impl TryFrom<RawExamSummary> for ExamSummary {
    type Error = ProtocolError;

    fn try_from(raw: RawExamSummary) -> Result<Self, Self::Error> {
        Self::new(raw.id, raw.title, raw.start_at, raw.end_at, raw.phase)
    }
}

#[derive(Deserialize)]
struct RawExamSummary {
    id: Uuid,
    title: String,
    #[serde(with = "crate::utc::rfc3339")]
    start_at: OffsetDateTime,
    #[serde(with = "crate::utc::rfc3339")]
    end_at: OffsetDateTime,
    phase: ExamPhase,
}

/// The response body of `GET /api/exams`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExamListResponse {
    /// The current Accoding user, also used for WebSocket authentication.
    pub user: AccodingUser,
    /// Exams the user is enrolled in.
    pub items: Vec<ExamSummary>,
}

/// Phase of an exam, derived from the server clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExamPhase {
    /// The exam has not started.
    Scheduled,
    /// The exam is in progress.
    Running,
    /// The exam has ended.
    Ended,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp(value: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(value).expect("timestamp in range")
    }

    #[test]
    fn user_id_must_be_positive() {
        assert_eq!(
            AccodingUser::new(0, "user".to_owned(), "name".to_owned()),
            Err(ProtocolError::InvalidUserId)
        );
        assert!(AccodingUser::new(1, "user".to_owned(), "name".to_owned()).is_ok());
    }

    #[test]
    fn exam_summary_rejects_reversed_range() {
        let result = ExamSummary::new(
            Uuid::nil(),
            "exam".to_owned(),
            timestamp(10),
            timestamp(5),
            ExamPhase::Scheduled,
        );
        assert_eq!(result, Err(ProtocolError::InvalidTimeRange));
    }

    #[test]
    fn times_are_normalized_to_utc() {
        let offset = time::UtcOffset::from_hms(8, 0, 0).expect("valid offset");
        let start = timestamp(0).to_offset(offset);
        let end = timestamp(3600).to_offset(offset);
        let summary = ExamSummary::new(
            Uuid::nil(),
            "exam".to_owned(),
            start,
            end,
            ExamPhase::Running,
        )
        .expect("valid summary");
        assert_eq!(summary.start_at.offset(), UtcOffset::UTC);
        assert_eq!(summary.end_at.offset(), UtcOffset::UTC);
    }

    #[test]
    fn deserialization_validates_reversed_range() {
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000010",
            "title": "exam",
            "start_at": "2026-12-20T04:00:00Z",
            "end_at": "2026-12-20T01:00:00Z",
            "phase": "scheduled"
        }"#;
        assert!(serde_json::from_str::<ExamSummary>(json).is_err());
    }
}
