//! Recording parameters and manifests.

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

/// Summary of a finished recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawRecordingManifest")]
pub struct RecordingManifest {
    /// Sealed segments, ordered by ascending index.
    pub segments: Vec<SegmentMeta>,
    /// SHA-256 of the merged video file.
    pub merged_sha256: Sha256Digest,
}

impl RecordingManifest {
    /// Creates a manifest, requiring strictly increasing segment indices.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidSegmentOrder`] when indices are equal or
    /// decreasing.
    pub fn new(
        segments: Vec<SegmentMeta>,
        merged_sha256: Sha256Digest,
    ) -> Result<Self, ProtocolError> {
        if !segments
            .windows(2)
            .all(|pair| pair[0].index < pair[1].index)
        {
            return Err(ProtocolError::InvalidSegmentOrder);
        }
        Ok(Self {
            segments,
            merged_sha256,
        })
    }
}

impl TryFrom<RawRecordingManifest> for RecordingManifest {
    type Error = ProtocolError;

    fn try_from(raw: RawRecordingManifest) -> Result<Self, Self::Error> {
        Self::new(raw.segments, raw.merged_sha256)
    }
}

#[derive(Deserialize)]
struct RawRecordingManifest {
    segments: Vec<SegmentMeta>,
    merged_sha256: Sha256Digest,
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

    #[test]
    fn manifest_requires_ascending_indices() {
        let first = SegmentMeta::new(1, timestamp(0), timestamp(30), digest('a')).expect("segment");
        let duplicate =
            SegmentMeta::new(1, timestamp(30), timestamp(60), digest('b')).expect("segment");
        let second =
            SegmentMeta::new(2, timestamp(30), timestamp(60), digest('b')).expect("segment");

        assert_eq!(
            RecordingManifest::new(vec![first.clone(), duplicate], digest('c')),
            Err(ProtocolError::InvalidSegmentOrder)
        );
        assert_eq!(
            RecordingManifest::new(vec![second, first], digest('c')),
            Err(ProtocolError::InvalidSegmentOrder)
        );
        assert!(
            RecordingManifest::new(
                vec![
                    SegmentMeta::new(1, timestamp(0), timestamp(30), digest('a')).expect("segment"),
                    SegmentMeta::new(2, timestamp(30), timestamp(60), digest('b'))
                        .expect("segment"),
                ],
                digest('c')
            )
            .is_ok()
        );
    }
}
