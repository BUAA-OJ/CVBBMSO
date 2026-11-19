//! Client-reported incidents.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Classification of a client incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IncidentCode {
    /// Recording failed.
    RecordingFailed,
    /// Recording recovered after a failure.
    RecordingRecovered,
    /// The controlled browser denied a navigation request.
    NavigationDenied,
}

/// One occurrence of a client incident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientIncident {
    /// Stable classification of the incident.
    pub code: IncidentCode,
    /// Time the incident occurred, normalized to UTC.
    #[serde(with = "crate::utc::rfc3339")]
    pub occurred_at: OffsetDateTime,
    /// Optional short context.
    pub detail: Option<String>,
}
