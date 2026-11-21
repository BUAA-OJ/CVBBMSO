//! Recording parameters, segments, and summaries.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset};

use crate::{ProtocolError, Sha256Digest};

/// Recording parameters fixed for the duration of an exam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawRecordingPolicy")]
pub struct RecordingPolicy {
    /// Target duration of one segment, in seconds.
    pub segment_duration_secs: u32,
    /// Capture frame rate, in frames per second.
    pub fps: u32,
}

impl RecordingPolicy {
    /// Creates a policy, rejecting zero values.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidRecordingPolicy`] when either value is
    /// zero.
    pub fn new(segment_duration_secs: u32, fps: u32) -> Result<Self, ProtocolError> {
        if segment_duration_secs == 0 || fps == 0 {
            return Err(ProtocolError::InvalidRecordingPolicy);
        }
        Ok(Self {
            segment_duration_secs,
            fps,
        })
    }
}

impl TryFrom<RawRecordingPolicy> for RecordingPolicy {
    type Error = ProtocolError;

    fn try_from(raw: RawRecordingPolicy) -> Result<Self, Self::Error> {
        Self::new(raw.segment_duration_secs, raw.fps)
    }
}

#[derive(Deserialize)]
struct RawRecordingPolicy {
    segment_duration_secs: u32,
    fps: u32,
}

/// Metadata of one sealed recording segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawSegmentMeta")]
pub struct SegmentMeta {
    /// One-based recording order index, increasing by one per segment.
    pub index: u32,
    /// Actual recording start time, normalized to UTC.
    #[serde(with = "crate::utc::rfc3339")]
    pub start_at: OffsetDateTime,
    /// Actual recording end time, normalized to UTC.
    #[serde(with = "crate::utc::rfc3339")]
    pub end_at: OffsetDateTime,
    /// SHA-256 of the sealed segment file.
    pub sha256: Sha256Digest,
}

impl SegmentMeta {
    /// Creates segment metadata, requiring `index >= 1` and
    /// `start_at <= end_at`.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidSegmentIndex`] when `index` is zero and
    /// [`ProtocolError::InvalidTimeRange`] when `start_at` is later than
    /// `end_at`.
    pub fn new(
        index: u32,
        start_at: OffsetDateTime,
        end_at: OffsetDateTime,
        sha256: Sha256Digest,
    ) -> Result<Self, ProtocolError> {
        if index == 0 {
            return Err(ProtocolError::InvalidSegmentIndex);
        }
        let start_at = start_at.to_offset(UtcOffset::UTC);
        let end_at = end_at.to_offset(UtcOffset::UTC);
        if start_at > end_at {
            return Err(ProtocolError::InvalidTimeRange);
        }
        Ok(Self {
            index,
            start_at,
            end_at,
            sha256,
        })
    }
}

impl TryFrom<RawSegmentMeta> for SegmentMeta {
    type Error = ProtocolError;

    fn try_from(raw: RawSegmentMeta) -> Result<Self, Self::Error> {
        Self::new(raw.index, raw.start_at, raw.end_at, raw.sha256)
    }
}

#[derive(Deserialize)]
struct RawSegmentMeta {
    index: u32,
    #[serde(with = "crate::utc::rfc3339")]
    start_at: OffsetDateTime,
    #[serde(with = "crate::utc::rfc3339")]
    end_at: OffsetDateTime,
    sha256: Sha256Digest,
}

/// Number of sealed segments and SHA-256 of the merged recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingSummary {
    /// Number of sealed segments.
    pub segment_count: NonZeroU32,
    /// SHA-256 of the merged video file.
    pub merged_sha256: Sha256Digest,
}

impl RecordingSummary {
    /// Creates a summary for a recording with at least one sealed segment.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidSegmentCount`] when the count is zero.
    pub fn new(segment_count: u32, merged_sha256: Sha256Digest) -> Result<Self, ProtocolError> {
        let segment_count =
            NonZeroU32::new(segment_count).ok_or(ProtocolError::InvalidSegmentCount)?;
        Ok(Self {
            segment_count,
            merged_sha256,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp(value: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(value).expect("timestamp in range")
    }

    fn digest(byte: char) -> Sha256Digest {
        Sha256Digest::parse(&byte.to_string().repeat(64)).expect("valid digest")
    }

    #[test]
    fn policy_values_must_be_positive() {
        assert_eq!(
            RecordingPolicy::new(0, 15),
            Err(ProtocolError::InvalidRecordingPolicy)
        );
        assert_eq!(
            RecordingPolicy::new(30, 0),
            Err(ProtocolError::InvalidRecordingPolicy)
        );
        assert_eq!(
            RecordingPolicy::new(30, 15),
            Ok(RecordingPolicy {
                segment_duration_secs: 30,
                fps: 15,
            })
        );
    }

    #[test]
    fn policy_deserialization_validates() {
        assert!(
            serde_json::from_str::<RecordingPolicy>(r#"{"segment_duration_secs":0,"fps":15}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<RecordingPolicy>(r#"{"segment_duration_secs":30,"fps":15}"#)
                .is_ok()
        );
    }

    #[test]
    fn segment_index_starts_at_one() {
        let result = SegmentMeta::new(0, timestamp(0), timestamp(30), digest('a'));
        assert_eq!(result, Err(ProtocolError::InvalidSegmentIndex));
    }

    #[test]
    fn segment_range_must_be_ordered() {
        let result = SegmentMeta::new(1, timestamp(30), timestamp(0), digest('a'));
        assert_eq!(result, Err(ProtocolError::InvalidTimeRange));
    }
}
