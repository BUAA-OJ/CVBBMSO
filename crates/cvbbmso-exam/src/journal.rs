//! Durable runtime journal of one candidate.
//!
//! The journal owns the reliable event queue: every event is written here
//! before it is sent, and stays until the exam closes with its final
//! `CommandAck`. The server's `sync.ack_event_through` controls replay.
//! It also records which recorder segments and summary were already queued
//! and the client state reached, so a restarted process resumes where it
//! stopped.
//!
//! Sequence numbers are anchored by the first `sync` this journal sees, not
//! assigned from 1 locally. A journal that was lost or reset therefore cannot
//! reuse sequence numbers the server already holds, which the server would
//! reject as conflicting duplicates.

use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    io::{self, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use cvbbmso_protocol::{
    ClientMessage, ClientState, ExamEvent, ExecutionState, RecordingState, RecordingSummary,
    SegmentMeta, Sha256Digest,
};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use uuid::Uuid;

use crate::ExamError;

/// Subdirectory of the candidate directory owned by the runtime.
const RUNTIME_DIR: &str = "runtime";
/// Journal snapshot file.
const JOURNAL_FILE: &str = "journal.json";
/// Name a corrupt snapshot is moved to before the journal starts afresh.
const CORRUPT_FILE: &str = "journal.json.corrupt";
const MAX_WIRE_SEQ: u64 = i64::MAX.cast_unsigned();
/// Old snapshots can store numbers above the current wire limit.
const LEGACY_EXHAUSTED: u128 = (u64::MAX as u128) + 1;

/// Resolves local storage for the verified candidate.
pub(crate) fn candidate_dir(
    app_data_dir: &Path,
    exam_id: Uuid,
    oj_user_id: i64,
) -> Result<PathBuf, ExamError> {
    if oj_user_id <= 0 {
        return Err(ExamError::Storage(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Accoding user id must be positive",
        )));
    }
    Ok(app_data_dir
        .join("exams")
        .join(exam_id.to_string())
        .join(oj_user_id.to_string()))
}

/// Durable runtime state of one candidate in one exam.
pub(crate) struct Journal {
    dir: PathBuf,
    data: Data,
}

#[derive(Debug, Serialize, Deserialize)]
struct Data {
    /// Next sequence number, including past the wire limit; `None` before the first sync.
    next_seq: Option<u128>,
    /// Server watermark covering any gaps in retained sequence numbers.
    #[serde(default)]
    ack_event_through: u64,
    /// Reliable events retained until the final command acknowledgement.
    queue: VecDeque<Entry>,
    /// Segment indexes already queued, so recovered segments are not repeated.
    segments: BTreeSet<u32>,
    /// Whether the recording summary was already queued.
    finalized: bool,
    /// Client state reached before the last save.
    state: ClientState,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// Keep the old unsigned representation to preserve existing snapshots.
    seq: Option<u64>,
    #[serde(deserialize_with = "deserialize_stored_event")]
    event: ExamEvent,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredEvent {
    Current(ExamEvent),
    Legacy(LegacyEvent),
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum LegacyEvent {
    RecordingFinalized {
        segments: Vec<SegmentMeta>,
        merged_sha256: Sha256Digest,
    },
}

// Old manifests duplicated the individually journaled segments. Convert only
// the stored summary payload, leaving its event number and the segment entries intact.
fn deserialize_stored_event<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<ExamEvent, D::Error> {
    match StoredEvent::deserialize(deserializer)? {
        StoredEvent::Current(event) => Ok(event),
        StoredEvent::Legacy(LegacyEvent::RecordingFinalized {
            segments,
            merged_sha256,
        }) => {
            let count = u32::try_from(segments.len()).map_err(D::Error::custom)?;
            if !segments.iter().map(|segment| segment.index).eq(1..=count) {
                return Err(D::Error::custom("noncontiguous legacy recording segments"));
            }
            RecordingSummary::new(count, merged_sha256)
                .map(ExamEvent::RecordingFinalized)
                .map_err(D::Error::custom)
        }
    }
}

impl Default for Data {
    fn default() -> Self {
        Self {
            next_seq: None,
            ack_event_through: 0,
            queue: VecDeque::new(),
            segments: BTreeSet::new(),
            finalized: false,
            state: ClientState {
                execution: ExecutionState::Ready,
                recording: RecordingState::Idle,
            },
        }
    }
}

impl Data {
    fn is_valid(&self) -> bool {
        use ExecutionState::{Finalizing, Finished, Ready, Recording};
        use RecordingState::{Capturing, Complete, Failed, Idle};

        let valid_state = match (self.state.execution, self.state.recording) {
            (Ready, Idle)
            | (Recording, Capturing | Failed)
            | (Finalizing, RecordingState::Finalizing | Failed)
            | (Finished, Failed) => true,
            (Finalizing | Finished, Complete) => self.finalized,
            _ => false,
        };
        valid_state && self.has_valid_numbering()
    }

    fn has_valid_numbering(&self) -> bool {
        let after_ack = u128::from(self.ack_event_through) + 1;
        if self.ack_event_through > MAX_WIRE_SEQ
            || match self.next_seq {
                Some(next) => !(after_ack..=LEGACY_EXHAUSTED).contains(&next),
                None => self.ack_event_through != 0,
            }
        {
            return false;
        }
        let mut expected = self.next_seq;
        for entry in self.queue.iter().rev() {
            match entry.seq {
                Some(seq)
                    if seq > 0
                        && expected.is_some_and(|next| {
                            let seq = u128::from(seq);
                            seq + 1 == next || (seq < next && next <= after_ack)
                        }) =>
                {
                    expected = Some(u128::from(seq));
                }
                None if expected == self.next_seq
                    && expected.is_none_or(|next| next > u128::from(MAX_WIRE_SEQ)) => {}
                _ => return false,
            }
        }
        true
    }
}

fn take_sequence(next: &mut Option<u128>) -> Option<u64> {
    let next = next.as_mut()?;
    let seq = u64::try_from(*next)
        .ok()
        .filter(|&seq| seq <= MAX_WIRE_SEQ)?;
    *next += 1;
    Some(seq)
}

impl Journal {
    /// Opens the journal under `candidate_dir`, creating it when absent.
    ///
    /// A snapshot that cannot be decoded is moved aside and replaced by an
    /// empty journal: the candidate must still be able to sit the exam, and
    /// anchored numbering keeps the fresh journal consistent with the server.
    pub(crate) async fn open(candidate_dir: &Path) -> Result<Self, ExamError> {
        let dir = candidate_dir.join(RUNTIME_DIR);
        let blocking_dir = dir.clone();
        let data = tokio::task::spawn_blocking(move || load(&blocking_dir))
            .await
            .map_err(|error| ExamError::Storage(io::Error::other(error)))?
            .map_err(ExamError::Storage)?;
        Ok(Self { dir, data })
    }

    /// Reads a pending journal without creating or repairing local data.
    pub(crate) async fn open_pending(candidate_dir: &Path) -> Result<Option<Self>, ExamError> {
        let dir = candidate_dir.join(RUNTIME_DIR);
        tokio::task::spawn_blocking(move || {
            let Some(bytes) = read_snapshot(&dir.join(JOURNAL_FILE))? else {
                return Ok(None);
            };
            let journal = Self {
                dir,
                data: decode_snapshot(&bytes)?,
            };
            Ok(journal.has_pending_finalization().then_some(journal))
        })
        .await
        .map_err(|error| ExamError::Storage(io::Error::other(error)))?
        .map_err(ExamError::Storage)
    }

    pub(crate) fn has_pending_finalization(&self) -> bool {
        matches!(
            self.state().execution,
            ExecutionState::Recording | ExecutionState::Finalizing
        )
    }

    /// Writes the current state durably.
    ///
    /// Callers save after every change and before sending the corresponding
    /// message, so nothing reaches the server that a crash could forget.
    pub(crate) async fn save(&self) -> Result<(), ExamError> {
        let bytes = serde_json::to_vec(&self.data)
            .map_err(|error| ExamError::Storage(io::Error::other(error)))?;
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || write_atomically(&dir, &bytes))
            .await
            .map_err(|error| ExamError::Storage(io::Error::other(error)))?
            .map_err(ExamError::Storage)
    }

    /// Queues an event and returns its sequence number, if already anchored.
    ///
    /// Returns `None` for a segment whose index was already queued, for a
    /// second recording summary, and for any event recorded before numbering
    /// is anchored or after the sequence space is exhausted.
    pub(crate) fn append(&mut self, event: ExamEvent) -> Option<i64> {
        let repeated = match &event {
            ExamEvent::SegmentFinalized(meta) => !self.data.segments.insert(meta.index),
            ExamEvent::RecordingFinalized(_) => std::mem::replace(&mut self.data.finalized, true),
            _ => false,
        };
        if repeated {
            return None;
        }
        let seq = take_sequence(&mut self.data.next_seq);
        self.data.queue.push_back(Entry { seq, event });
        seq.and_then(|seq| i64::try_from(seq).ok())
    }

    /// Applies `sync.ack_event_through` and numbers any unnumbered events.
    ///
    /// The first call anchors numbering after the server's position. Later
    /// calls never move numbering backwards.
    pub(crate) fn anchor(&mut self, ack_event_through: i64) {
        let Ok(ack_event_through) = u64::try_from(ack_event_through) else {
            return;
        };
        self.data.ack_event_through = self.data.ack_event_through.max(ack_event_through);
        let after_ack = u128::from(ack_event_through) + 1;
        self.data.next_seq = Some(self.data.next_seq.unwrap_or(after_ack).max(after_ack));

        for entry in &mut self.data.queue {
            if entry.seq.is_none() {
                entry.seq = take_sequence(&mut self.data.next_seq);
            }
        }
    }

    /// Whether the queue contains events that cannot yet be sent on the wire.
    pub(crate) fn has_unsendable_events(&self) -> bool {
        self.data
            .queue
            .back()
            .is_some_and(|entry| entry.seq.is_none_or(|seq| seq > MAX_WIRE_SEQ))
    }

    /// Whether all segments from the recorder's summary have been journaled.
    pub(crate) fn has_recording_segments(&self, count: NonZeroU32) -> bool {
        self.data.segments.iter().copied().eq(1..=count.get())
    }

    /// Returns numbered messages beyond the current session's send position.
    pub(crate) fn messages_after(
        &self,
        through: i64,
    ) -> impl Iterator<Item = (i64, ClientMessage)> + '_ {
        self.data.queue.iter().filter_map(move |entry| {
            let seq = i64::try_from(entry.seq?)
                .ok()
                .filter(|&seq| seq > through)?;
            Some((
                seq,
                ClientMessage::Event {
                    seq,
                    event: entry.event.clone(),
                },
            ))
        })
    }

    /// Drops every queued event after the server confirmed the end of the exam.
    ///
    /// Numbering and the known segment indexes are kept, so a later restart
    /// neither reuses a sequence number nor reports a segment twice.
    pub(crate) fn clear_events(&mut self) {
        self.data.queue.clear();
    }

    /// Client state reached before the last save.
    pub(crate) fn state(&self) -> ClientState {
        self.data.state
    }

    /// Records the client state reached.
    pub(crate) fn set_state(&mut self, state: ClientState) {
        self.data.state = state;
    }
}

/// Reads the snapshot under `dir`, setting a corrupt one aside.
fn load(dir: &Path) -> io::Result<Data> {
    fs::create_dir_all(dir)?;
    let path = dir.join(JOURNAL_FILE);
    let Some(bytes) = read_snapshot(&path)? else {
        return Ok(Data::default());
    };
    match decode_snapshot(&bytes) {
        Ok(data) => Ok(data),
        Err(error) => recover_corrupt(dir, &path, &error),
    }
}

fn read_snapshot(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn decode_snapshot(bytes: &[u8]) -> io::Result<Data> {
    let data = serde_json::from_slice::<Data>(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !data.is_valid() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "inconsistent runtime journal",
        ));
    }
    Ok(data)
}

fn recover_corrupt(dir: &Path, path: &Path, error: &impl std::fmt::Display) -> io::Result<Data> {
    tracing::warn!(
        path = %path.display(),
        %error,
        "runtime journal is corrupt; starting an empty journal"
    );
    fs::rename(path, dir.join(CORRUPT_FILE))?;
    Ok(Data::default())
}

/// Replaces the snapshot without following a pre-existing temporary path.
fn write_atomically(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temporary = tempfile::Builder::new()
        .prefix(".journal-")
        .tempfile_in(dir)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(dir.join(JOURNAL_FILE))
        .map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests;
