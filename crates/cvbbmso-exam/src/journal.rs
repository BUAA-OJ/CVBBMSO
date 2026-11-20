//! Durable runtime journal of one candidate.
//!
//! The journal owns the reliable event queue: every event is written here
//! before it is sent, and stays until the exam closes with its final
//! `CommandAck`. The server's `sync.ack_event_through` controls replay.
//! It also records which recorder segments and manifest were already queued
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
    path::{Path, PathBuf},
};

use cvbbmso_protocol::{ClientMessage, ClientState, ExamEvent, ExecutionState, RecordingState};
use serde::{Deserialize, Serialize};

use crate::ExamError;

/// Subdirectory of the candidate directory owned by the runtime.
const RUNTIME_DIR: &str = "runtime";
/// Journal snapshot file.
const JOURNAL_FILE: &str = "journal.json";
/// Name a corrupt snapshot is moved to before the journal starts afresh.
const CORRUPT_FILE: &str = "journal.json.corrupt";
/// One past the last wire sequence number, representing exhaustion.
const EXHAUSTED: i128 = (i64::MAX as i128) + 1;

/// Durable runtime state of one candidate in one exam.
pub(crate) struct Journal {
    dir: PathBuf,
    data: Data,
}

#[derive(Debug, Serialize, Deserialize)]
struct Data {
    /// Next sequence number or `EXHAUSTED`; `None` until the first sync.
    next_seq: Option<i128>,
    /// Reliable events in recording order.
    queue: VecDeque<Entry>,
    /// Segment indexes already queued, so recovered segments are not repeated.
    segments: BTreeSet<u32>,
    /// Whether the recording manifest was already queued.
    finalized: bool,
    /// Client state reached before the last save.
    state: ClientState,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    seq: Option<i64>,
    event: ExamEvent,
}

impl Default for Data {
    fn default() -> Self {
        Self {
            next_seq: None,
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
        if self
            .next_seq
            .is_some_and(|seq| !(1..=EXHAUSTED).contains(&seq))
        {
            return false;
        }
        let mut expected = self.next_seq;
        for entry in self.queue.iter().rev() {
            match entry.seq {
                Some(seq) if seq > 0 && expected == Some(i128::from(seq) + 1) => {
                    expected = Some(i128::from(seq));
                }
                None if expected == self.next_seq && matches!(expected, None | Some(EXHAUSTED)) => {
                }
                _ => return false,
            }
        }
        true
    }
}

fn take_sequence(next: &mut Option<i128>) -> Option<i64> {
    let next = next.as_mut()?;
    let seq = i64::try_from(*next).ok()?;
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
    /// second recording manifest, and for any event recorded before numbering
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
        seq
    }

    /// Applies `sync.ack_event_through` and numbers any unnumbered events.
    ///
    /// The first call anchors numbering after the server's position. Later
    /// calls never move numbering backwards.
    pub(crate) fn anchor(&mut self, ack_event_through: i64) {
        let after_ack = i128::from(ack_event_through) + 1;
        let next = self.data.next_seq.unwrap_or(after_ack);
        // A server position ahead of this journal supersedes the local numbering.
        if after_ack > next {
            self.data
                .queue
                .retain(|entry| entry.seq.is_none_or(|seq| seq > ack_event_through));
        }
        self.data.next_seq = Some(next.max(after_ack));

        for entry in &mut self.data.queue {
            if entry.seq.is_none() {
                entry.seq = take_sequence(&mut self.data.next_seq);
            }
        }
    }

    /// Whether events are still waiting for a sequence number.
    pub(crate) fn has_unnumbered_events(&self) -> bool {
        self.data
            .queue
            .back()
            .is_some_and(|entry| entry.seq.is_none())
    }

    /// Returns numbered messages beyond the current session's send position.
    pub(crate) fn messages_after(
        &self,
        through: i64,
    ) -> impl Iterator<Item = (i64, ClientMessage)> + '_ {
        self.data.queue.iter().filter_map(move |entry| {
            let seq = entry.seq.filter(|&seq| seq > through)?;
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
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Data::default()),
        Err(error) => return Err(error),
    };
    match serde_json::from_slice::<Data>(&bytes) {
        Ok(data) if data.is_valid() => Ok(data),
        Ok(_) => recover_corrupt(dir, &path, &"inconsistent snapshot"),
        Err(error) => recover_corrupt(dir, &path, &error),
    }
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
