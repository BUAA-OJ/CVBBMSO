//! Timestamp serde support shared by every DTO.

use serde::{Deserializer, Serializer};
use time::{OffsetDateTime, UtcOffset};

/// RFC 3339 timestamps that are always rendered and returned in UTC.
pub(crate) mod rfc3339 {
    use super::{Deserializer, OffsetDateTime, Serializer, UtcOffset};

    pub(crate) fn serialize<S>(value: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        time::serde::rfc3339::serialize(&value.to_offset(UtcOffset::UTC), serializer)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        time::serde::rfc3339::deserialize(deserializer).map(|value| value.to_offset(UtcOffset::UTC))
    }
}
