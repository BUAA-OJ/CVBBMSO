//! Sealed segment files of one candidate recording.

// Sealing is driven by the capture pipeline, which lands with the platform
// milestone; until then the sealing path is reached only from tests.
#![allow(dead_code)]

mod recovery;

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use cvbbmso_protocol::{ProtocolError, RecordingManifest, SegmentMeta, Sha256Digest};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::RecordingError;

/// Directory holding every recording artifact of one candidate.
const RECORDING_DIR: &str = "recording";
/// Subdirectory holding sealed segments.
const SEGMENTS_DIR: &str = "segments";
/// Extension of a sealed segment video.
const VIDEO_EXTENSION: &str = "mkv";
/// Extension of the metadata file of a sealed segment.
const META_EXTENSION: &str = "json";
/// Extension of the temporary file used to publish metadata atomically.
const TEMP_EXTENSION: &str = "tmp";
/// Number of digits of the zero-padded segment ordinal in file names.
const ORDINAL_WIDTH: usize = 6;
/// Buffer used to hash a segment without loading it whole.
const HASH_BUFFER_SIZE: usize = 64 * 1024;

/// Sealed segments of one candidate, ordered by their one-based index.
pub(crate) struct SegmentStore {
    recording_dir: PathBuf,
    segments: BTreeMap<u32, SegmentMeta>,
}

impl SegmentStore {
    /// Opens `<candidate_dir>/recording`, creating it when missing, and restores
    /// the segments sealed by an earlier session.
    pub(crate) fn open(candidate_dir: &Path) -> Result<Self, RecordingError> {
        let recording_dir = candidate_dir.join(RECORDING_DIR);
        let segments_dir = recording_dir.join(SEGMENTS_DIR);
        fs::create_dir_all(&segments_dir)?;
        let segments = recovery::recover(&segments_dir)?;
        Ok(Self {
            recording_dir,
            segments,
        })
    }

    /// Sealed segments in ascending index order.
    #[must_use]
    pub(crate) fn segments(&self) -> Vec<SegmentMeta> {
        self.segments.values().cloned().collect()
    }

    /// Index the next sealed segment must use.
    #[must_use]
    pub(crate) fn next_index(&self) -> u32 {
        self.segments
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// Path of the video of `index`, whether or not it exists yet.
    #[must_use]
    pub(crate) fn video_path(&self, index: u32) -> PathBuf {
        self.segments_dir().join(file_name(index, VIDEO_EXTENSION))
    }

    /// Seals the finished video file of `index` and persists its metadata.
    ///
    /// The caller must have finished writing the video file. The digest is
    /// computed before the metadata is published atomically, so a metadata file
    /// always describes a complete segment.
    pub(crate) fn seal(
        &mut self,
        index: u32,
        start_at: OffsetDateTime,
        end_at: OffsetDateTime,
    ) -> Result<SegmentMeta, RecordingError> {
        let next = self.next_index();
        if index != next {
            return Err(invalid_input(format!(
                "segment index {index} does not follow {next}"
            )));
        }
        let digest = hash_file(&self.video_path(index))?;
        let meta = SegmentMeta::new(index, start_at, end_at, digest).map_err(rejected_segment)?;
        let json = serde_json::to_vec(&meta).map_err(io::Error::other)?;
        write_atomically(&self.meta_path(index), &json)?;
        self.segments.insert(index, meta.clone());
        Ok(meta)
    }

    /// Builds the manifest of the sealed segments.
    pub(crate) fn manifest(
        &self,
        merged_sha256: Sha256Digest,
    ) -> Result<RecordingManifest, RecordingError> {
        RecordingManifest::new(self.segments(), merged_sha256).map_err(rejected_segment)
    }

    fn segments_dir(&self) -> PathBuf {
        self.recording_dir.join(SEGMENTS_DIR)
    }

    fn meta_path(&self, index: u32) -> PathBuf {
        self.segments_dir().join(file_name(index, META_EXTENSION))
    }
}

/// Renders the zero-padded file name of one segment file.
fn file_name(index: u32, extension: &str) -> String {
    format!("{index:0ORDINAL_WIDTH$}.{extension}")
}

/// Publishes `bytes` at `path` by writing a sibling temporary file first.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), RecordingError> {
    let temporary = path.with_extension(TEMP_EXTENSION);
    let mut file = File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    Ok(())
}

/// Streams `path` through SHA-256.
fn hash_file(path: &Path) -> Result<Sha256Digest, RecordingError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    Sha256Digest::parse(&hexadecimal(&digest)).map_err(rejected_segment)
}

/// Renders bytes as lowercase hexadecimal text.
fn hexadecimal(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Wraps a rejected protocol invariant as a storage error.
///
/// Segment metadata is derived from values the recorder controls, so a rejection
/// means the on-disk or in-memory state is inconsistent.
fn rejected_segment(error: ProtocolError) -> RecordingError {
    RecordingError::Io(io::Error::new(io::ErrorKind::InvalidInput, error))
}

/// Builds a storage error for a violated recording-layout invariant.
fn invalid_input(message: impl Into<String>) -> RecordingError {
    RecordingError::Io(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
}

#[cfg(test)]
mod tests;
