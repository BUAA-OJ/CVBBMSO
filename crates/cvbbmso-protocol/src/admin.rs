//! Administrative HTTP payloads, separate from the student protocol.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{ExamEvent, ExamSummary, ExecutionState, RecordingPolicy};

/// Request to create a scheduled exam.
#[derive(Debug, Deserialize)]
pub struct CreateExam {
    /// Human-readable exam title.
    pub title: String,
    /// Scheduled start time, normalized to UTC.
    #[serde(with = "crate::utc::rfc3339")]
    pub start_at: OffsetDateTime,
    /// Scheduled end time, normalized to UTC.
    #[serde(with = "crate::utc::rfc3339")]
    pub end_at: OffsetDateTime,
    /// Recording settings for the exam.
    pub policy: RecordingPolicy,
}

/// Identity of a newly created exam.
#[derive(Debug, Serialize)]
pub struct CreatedExam {
    /// Assigned exam ID.
    pub exam_id: Uuid,
}

/// Exam settings visible to staff.
#[derive(Debug, Serialize)]
pub struct ExamDetail {
    /// Exam identity, schedule, and phase.
    pub summary: ExamSummary,
    /// Fixed recording settings.
    pub policy: RecordingPolicy,
}

/// Request to add one or more participants idempotently.
#[derive(Debug, Deserialize)]
pub struct ImportParticipants {
    /// Decimal Accoding user IDs encoded as strings to preserve 64-bit precision.
    pub oj_user_ids: Vec<String>,
}

/// Request to enroll students by their school student numbers.
#[derive(Debug, Deserialize)]
pub struct ImportStudentEnrollments {
    /// Student numbers as decimal strings, including any leading zeros.
    pub student_numbers: Vec<String>,
}

/// A school roster invitation and its verified account binding.
#[derive(Debug, Serialize)]
pub struct StudentEnrollment {
    /// Decimal school student number.
    pub student_number: String,
    /// Decimal Accoding user ID, or null before the student enters.
    pub oj_user_id: Option<String>,
    /// Whether the invitation is active.
    pub enrolled: bool,
}

/// Result of importing participants.
#[derive(Debug, Serialize)]
pub struct ImportedParticipants {
    /// Number of newly created enrollments.
    pub count: usize,
}

/// A current enrollment or a student with recorded runtime history.
#[derive(Debug, Serialize)]
pub struct RosterEntry {
    /// Decimal Accoding user ID.
    pub oj_user_id: String,
    /// Whether the student remains enrolled.
    pub enrolled: bool,
    /// Runtime details, or `null` if the student has never connected.
    pub runtime: Option<StudentStatus>,
}

/// Last known runtime for a student.
#[derive(Debug, Serialize)]
pub struct StudentStatus {
    /// Whether the server has a live connection for the student.
    pub connected: bool,
    /// Last persisted execution stage.
    pub execution: ExecutionState,
    /// Most recent connection or heartbeat time in UTC.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen_at: Option<OffsetDateTime>,
}

/// An incident the server or client recorded during an exam.
#[derive(Debug, Serialize)]
pub struct Incident {
    /// Incident identity.
    pub id: Uuid,
    /// Decimal Accoding user ID.
    pub oj_user_id: String,
    /// Origin of the incident.
    pub source: IncidentSource,
    /// Stable incident kind.
    pub kind: String,
    /// When the incident occurred.
    #[serde(with = "crate::utc::rfc3339")]
    pub occurred_at: OffsetDateTime,
    /// Optional diagnostic detail.
    pub detail: Option<String>,
}

/// Where an exam incident was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IncidentSource {
    /// Detected by the server.
    Server,
    /// Reported by the student client.
    Client,
}

/// Limits incident history to one student when specified.
#[derive(Debug, Deserialize)]
pub struct IncidentFilter {
    /// Return only incidents affecting this Accoding user ID when provided.
    pub oj_user_id: Option<i64>,
}

/// A reliable event persisted for a student.
#[derive(Debug, Serialize)]
pub struct Event {
    /// Original event sequence number encoded as a decimal string.
    pub seq: String,
    /// Event body received from the student client.
    pub event: ExamEvent,
    /// When the server received the event.
    #[serde(with = "crate::utc::rfc3339")]
    pub received_at: OffsetDateTime,
}

/// Error body for a rejected admin API operation.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    /// Stable uppercase error code.
    pub code: &'static str,
}
