//! Sealed segment files of one candidate recording.

mod lock;
mod merge;
mod recovery;

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use cvbbmso_protocol::{ProtocolError, RecordingManifest, SegmentMeta, Sha256Digest};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::RecordingError;

/// Directory holding every recording artifact of one candidate.
const RECORDING_DIR: &str = "recording";
/// Subdirectory holding sealed segments.
const SEGMENTS_DIR: &str = "segments";
/// Name of the merged recording.
const FINAL_NAME: &str = "final.mp4";
/// Extension of a sealed segment video.
const VIDEO_EXTENSION: &str = "mkv";
/// Extension of the metadata file of a sealed segment.
const META_EXTENSION: &str = "json";
/// Extension of temporary files used for atomic publication.
const TEMP_EXTENSION: &str = "tmp";
/// Number of digits of the zero-padded segment ordinal in file names.
const ORDINAL_WIDTH: usize = 6;
/// Buffer used to hash a segment without loading it whole.
const HASH_BUFFER_SIZE: usize = 64 * 1024;
/// Upper bound on the bytes read into memory for one sealed segment video.
///
/// Segments rotate at the policy duration and the encoder targets 4 Mbit/s, so
/// a normal segment is only a few megabytes. This bound leaves ample room for
/// hardware encoders that overshoot the target or for longer rotations while
/// keeping the whole-file parse from buffering an arbitrary artifact.
pub(crate) const MAX_SEGMENT_BYTES: u64 = 512 * 1024 * 1024;
/// Upper bound on the bytes read into memory for one segment metadata file.
///
/// `SegmentMeta` is a fixed record of about 150 serialized bytes. 64 KiB leaves
/// room for whitespace and future fields while still bounding a file crafted to
/// exhaust memory during recovery.
const MAX_SEGMENT_METADATA_BYTES: u64 = 64 * 1024;
/// Distinguishes the temporary files created by this process.
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Upper bounds on the bytes read into memory for one recording artifact.
#[derive(Clone, Copy)]
pub(crate) struct ReadLimits {
    /// Limit for one sealed segment video.
    pub(crate) segment_video: u64,
    /// Limit for one segment metadata file.
    pub(crate) segment_metadata: u64,
}

impl ReadLimits {
    /// Bounds applied to every real recording.
    pub(crate) const DEFAULT: Self = Self {
        segment_video: MAX_SEGMENT_BYTES,
        segment_metadata: MAX_SEGMENT_METADATA_BYTES,
    };

    /// Builds small bounds for tests that verify the exact boundary.
    #[cfg(test)]
    pub(crate) const fn new(segment_video: u64, segment_metadata: u64) -> Self {
        Self {
            segment_video,
            segment_metadata,
        }
    }
}

/// Sealed segments of one candidate, ordered by their one-based index.
#[derive(Clone)]
pub(crate) struct SegmentStore {
    recording_dir: PathBuf,
    segments: BTreeMap<u32, SegmentMeta>,
    limits: ReadLimits,
    temporary: PathBuf,
    // Store snapshots keep the directory owned while a blocking task is live.
    _lock: Arc<File>,
}

impl SegmentStore {
    /// Opens `<candidate_dir>/recording`, creating it when missing, and restores
    /// the segments sealed by an earlier session.
    pub(crate) fn open(candidate_dir: &Path) -> Result<Self, RecordingError> {
        Self::open_with_limits(candidate_dir, ReadLimits::DEFAULT)
    }

    /// Opens the recording directory with explicit read bounds.
    ///
    /// Tests use smaller bounds to verify the boundary without writing
    /// production-sized artifacts; production callers use [`ReadLimits::DEFAULT`].
    pub(crate) fn open_with_limits(
        candidate_dir: &Path,
        limits: ReadLimits,
    ) -> Result<Self, RecordingError> {
        let recording_dir = candidate_dir.join(RECORDING_DIR);
        let segments_dir = recording_dir.join(SEGMENTS_DIR);
        fs::create_dir_all(&segments_dir)?;
        let lock = Arc::new(lock::open(&recording_dir.join(".lock"))?);
        // Temporary files are not removed here. A merge task may still be
        // writing one, and an interrupted attempt's file must never be adopted
        // or truncated by recovery.
        let segments = recovery::recover(&segments_dir, limits.segment_metadata)?;
        // A crashed process can leave its temporary file behind. Skip any name
        // this process would reuse so the stale file never blocks a merge.
        let temporary = loop {
            let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate = recording_dir.join(format!(
                "{FINAL_NAME}.{}.{sequence}.{TEMP_EXTENSION}",
                std::process::id()
            ));
            if !candidate.exists() {
                break candidate;
            }
        };
        Ok(Self {
            recording_dir,
            segments,
            limits,
            temporary,
            _lock: lock,
        })
    }

    /// Sealed segments in ascending index order.
    #[must_use]
    pub(crate) fn segments(&self) -> Vec<SegmentMeta> {
        self.segments.values().cloned().collect()
    }

    /// Recovers the disk state if a capture worker could not return its store.
    pub(crate) fn refresh(mut self) -> Result<Self, RecordingError> {
        self.segments = recovery::recover(&self.segments_dir(), self.limits.segment_metadata)?;
        Ok(self)
    }

    /// Path of the merged recording.
    #[must_use]
    pub(crate) fn final_path(&self) -> PathBuf {
        self.recording_dir.join(FINAL_NAME)
    }

    /// Path this store's merge attempt must create exclusively.
    #[must_use]
    pub(crate) fn final_temporary_path(&self) -> PathBuf {
        self.temporary.clone()
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

    /// Saves a complete MKV and publishes its metadata as the commit marker.
    pub(crate) fn write_segment(
        &mut self,
        bytes: &[u8],
        start_at: OffsetDateTime,
        end_at: OffsetDateTime,
    ) -> Result<SegmentMeta, RecordingError> {
        let index = self.next_index();
        self.require_unsealed(index)?;
        if bytes.len() as u64 > self.limits.segment_video {
            return Err(invalid_input("encoded segment exceeds the read limit"));
        }
        let video = self.video_path(index);
        preserve_unsealed_video(&video)?;
        let mut file = create_exclusive(&video)?;
        let saved = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        let result = saved
            .map_err(RecordingError::from)
            .and_then(|()| self.seal(index, start_at, end_at));
        if result.is_err() {
            // Only remove the unpublished video created by this call.
            let _ = fs::remove_file(&video);
        }
        result
    }

    /// Seals the finished video file of `index` and persists its metadata.
    ///
    /// The caller must have finished writing the video file. The digest is
    /// computed before the metadata is published atomically, so a metadata file
    /// always describes a complete segment.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Io`] when `index` is not the next index, when
    /// the segment is already sealed, or when the video cannot be hashed.
    pub(crate) fn seal(
        &mut self,
        index: u32,
        start_at: OffsetDateTime,
        end_at: OffsetDateTime,
    ) -> Result<SegmentMeta, RecordingError> {
        self.require_unsealed(index)?;
        let digest = hash_file(&self.video_path(index))?;
        let meta = SegmentMeta::new(index, start_at, end_at, digest).map_err(rejected_segment)?;
        let json = serde_json::to_vec(&meta).map_err(io::Error::other)?;
        write_atomically(&self.meta_path(index), &json)?;
        self.segments.insert(index, meta.clone());
        Ok(meta)
    }

    fn require_unsealed(&self, index: u32) -> Result<(), RecordingError> {
        let next = self.next_index();
        if index != next {
            return Err(invalid_input(format!(
                "segment index {index} does not follow {next}"
            )));
        }
        // A sealed segment is evidence: never publish over an existing one.
        match fs::symlink_metadata(self.meta_path(index)) {
            Ok(_) => Err(invalid_input(format!("segment {index} is already sealed"))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
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
    let (temporary, mut file) = loop {
        let temporary = sibling(path, TEMP_EXTENSION);
        match create_exclusive(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    };
    let saved = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let published = saved.and_then(|()| fs::hard_link(&temporary, path));
    let _ = fs::remove_file(&temporary);
    published.map_err(RecordingError::from)
}

fn create_exclusive(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{}.{sequence}.{suffix}", std::process::id()));
    PathBuf::from(name)
}

/// Keeps legacy or crash-left videos while making their unsealed index reusable.
fn preserve_unsealed_video(video: &Path) -> io::Result<()> {
    match fs::symlink_metadata(video) {
        Ok(meta) if !meta.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsealed video path is not a regular file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    loop {
        match fs::hard_link(video, sibling(video, "orphan")) {
            Ok(()) => return fs::remove_file(video),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

/// Reads `path` into memory, failing when it holds more than `limit` bytes.
///
/// The limit is enforced on the bytes actually read rather than on the file's
/// reported length, so a growing file cannot exceed the buffer. A file of
/// exactly `limit` bytes is accepted.
fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut reader = File::open(path)?.take(limit.saturating_add(1));
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds the {limit}-byte read limit", path.display()),
        ));
    }
    Ok(bytes)
}

/// Streams `path` through SHA-256.
fn hash_file(path: &Path) -> io::Result<Sha256Digest> {
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
    Sha256Digest::parse(&hexadecimal(&digest))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
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
