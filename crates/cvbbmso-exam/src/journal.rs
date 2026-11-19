//! Durable runtime journal of one candidate.
//!
//! The journal owns the reliable event queue: every event is written here
//! before it is sent, and stays until the server confirms it through
//! `sync.ack_event_through` or the exam closes with its final `CommandAck`.
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
    fs::{self, File},
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
/// Temporary file used to replace the snapshot atomically.
const TEMP_FILE: &str = "journal.json.tmp";
/// Name a corrupt snapshot is moved to before the journal starts afresh.
const CORRUPT_FILE: &str = "journal.json.corrupt";

/// Durable runtime state of one candidate in one exam.
pub(crate) struct Journal {
    dir: PathBuf,
    data: Data,
}

#[derive(Debug, Serialize, Deserialize)]
struct Data {
    /// Next sequence number; `None` until the first `sync` anchors numbering.
    next_seq: Option<u64>,
    /// Events not yet confirmed by the server, in recording order.
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
    seq: Option<u64>,
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

    /// Queues an event and returns the message to send once saved.
    ///
    /// Returns `None` for a segment whose index was already queued, for a
    /// second recording manifest, and for any event recorded before numbering
    /// is anchored; [`Journal::anchor`] numbers and returns those later.
    pub(crate) fn append(&mut self, event: ExamEvent) -> Option<ClientMessage> {
        let repeated = match &event {
            ExamEvent::SegmentFinalized(meta) => !self.data.segments.insert(meta.index),
            ExamEvent::RecordingFinalized(_) => std::mem::replace(&mut self.data.finalized, true),
            _ => false,
        };
        if repeated {
            return None;
        }
        let seq = self.data.next_seq;
        if let Some(seq) = seq {
            self.data.next_seq = Some(seq + 1);
        }
        let message = seq.map(|seq| ClientMessage::Event {
            seq,
            event: event.clone(),
        });
        self.data.queue.push_back(Entry { seq, event });
        message
    }

    /// Applies `sync.ack_event_through` and returns every unconfirmed event to
    /// send again, in order.
    ///
    /// The first call anchors numbering after the server's position. Later
    /// calls never move numbering backwards.
    pub(crate) fn anchor(&mut self, ack_event_through: u64) -> Vec<ClientMessage> {
        let after_ack = ack_event_through.saturating_add(1);
        let next = self
            .data
            .next_seq
            .map_or(after_ack, |next| next.max(after_ack));
        self.data
            .queue
            .retain(|entry| entry.seq.is_none_or(|seq| seq > ack_event_through));

        let mut next = next;
        for entry in &mut self.data.queue {
            if entry.seq.is_none() {
                entry.seq = Some(next);
                next += 1;
            }
        }
        self.data.next_seq = Some(next);

        self.data
            .queue
            .iter()
            .filter_map(|entry| {
                entry.seq.map(|seq| ClientMessage::Event {
                    seq,
                    event: entry.event.clone(),
                })
            })
            .collect()
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
    match serde_json::from_slice(&bytes) {
        Ok(data) => Ok(data),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "runtime journal is corrupt; starting an empty journal"
            );
            fs::rename(&path, dir.join(CORRUPT_FILE))?;
            Ok(Data::default())
        }
    }
}

/// Replaces the snapshot under `dir` without exposing a partial file.
fn write_atomically(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = dir.join(TEMP_FILE);
    let mut file = File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, dir.join(JOURNAL_FILE))
}

#[cfg(test)]
mod tests;
