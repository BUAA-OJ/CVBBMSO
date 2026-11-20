//! Recording lifecycle: opening a candidate, capturing, stopping, and finishing.

use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
};

use cvbbmso_protocol::{RecordingManifest, RecordingPolicy};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{RecorderEvent, RecordingError, store::SegmentStore};

/// Capacity reserved for live events beyond the restored segments.
const EVENT_BUFFER: usize = 16;

/// Recorder of one candidate in one exam.
pub struct Recorder {
    store: SegmentStore,
    events: Sender<RecorderEvent>,
    state: State,
}

/// Lifecycle position of a recorder, as described by the crate document.
enum State {
    /// The directory is open and no capture is running.
    Opened,
    /// Capture is running.
    #[allow(dead_code)] // Entered once the capture backend lands.
    Capturing,
    /// The last capture or finish attempt failed; the recorder can retry.
    Failed,
    /// The recorder owns the merge even if its caller stops awaiting it.
    Finalizing(JoinHandle<Result<RecordingManifest, RecordingError>>),
    /// Finishing succeeded; the manifest is cached so repeated calls agree.
    Complete(RecordingManifest),
}

impl Recorder {
    /// Opens the candidate recording directory and restores sealed segments.
    ///
    /// Restored segments are delivered as [`RecorderEvent::SegmentFinalized`] in
    /// ascending index order before this call returns, so the runtime can report
    /// them again after reconnecting.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Io`] when `candidate_dir` does not end in
    /// `<exam_id>/<oj_user_id>`, when the recording directory cannot be created,
    /// or when an existing segment cannot be read.
    pub async fn open(
        exam_id: Uuid,
        oj_user_id: i64,
        candidate_dir: PathBuf,
        _policy: RecordingPolicy,
    ) -> Result<(Self, Receiver<RecorderEvent>), RecordingError> {
        require_candidate_dir(&candidate_dir, exam_id, oj_user_id)?;
        // Recovery reads and hashes every sealed segment, so keep it off the
        // runtime worker instead of stalling unrelated tasks on disk latency.
        let directory = candidate_dir.clone();
        let store = tokio::task::spawn_blocking(move || SegmentStore::open(&directory))
            .await
            .map_err(io::Error::other)??;
        let restored = store.segments();
        let (events, receiver) = mpsc::channel(restored.len() + EVENT_BUFFER);
        let recorder = Self {
            store,
            events,
            state: State::Opened,
        };
        for meta in restored {
            recorder
                .events
                .send(RecorderEvent::SegmentFinalized(meta))
                .await
                .map_err(|_| io::Error::other("recorder event receiver was closed"))?;
        }
        Ok((recorder, receiver))
    }

    /// Starts or resumes primary-display capture.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Platform`] until the capture backend lands.
    /// Calling it while capture already runs succeeds without side effects;
    /// calling it after a failure retries the capture.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)] // Signatures are fixed by the crate contract.
    pub async fn start(&mut self) -> Result<(), RecordingError> {
        if matches!(self.state, State::Capturing) {
            return Ok(());
        }
        Err(RecordingError::Platform(None))
    }

    /// Stops capture and seals the segment being recorded.
    ///
    /// Merging and the manifest belong to [`Recorder::finish`]. Stopping an idle
    /// or already stopped recorder does nothing; stopping a failed capture drops
    /// the unsealed segment and keeps the sealed ones.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Io`] when the pending segment cannot be sealed.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)] // Signatures are fixed by the crate contract.
    pub async fn stop(&mut self) -> Result<(), RecordingError> {
        if matches!(self.state, State::Capturing | State::Failed) {
            self.state = State::Opened;
        }
        Ok(())
    }

    /// Stops capture, seals the pending segment, merges the recording, and
    /// returns its manifest.
    ///
    /// The merge runs on a blocking task because it reads, hashes, and writes
    /// whole segments; awaiting it does not block a Tokio worker.
    /// Cancelling this future only stops waiting: the recorder retains the
    /// task and its result, and the next call awaits the same merge. Keep this
    /// recorder alive until finishing completes; dropping it cannot stop an
    /// already running filesystem operation.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::NoSegments`] when nothing was recorded and
    /// [`RecordingError::Merge`] when the segments cannot be merged. A failed
    /// attempt leaves the recorder retryable; repeating the call after a
    /// successful finish returns the same manifest.
    pub async fn finish(&mut self) -> Result<RecordingManifest, RecordingError> {
        loop {
            match &mut self.state {
                State::Complete(manifest) => return Ok(manifest.clone()),
                State::Finalizing(task) => {
                    // Await by reference so cancelling the caller cannot detach
                    // the task or allow another writer of the merge temporary
                    // file.
                    let merged = task.await;
                    return match merged {
                        Ok(Ok(manifest)) => {
                            self.state = State::Complete(manifest.clone());
                            Ok(manifest)
                        }
                        Ok(Err(error)) => {
                            self.state = State::Failed;
                            Err(error)
                        }
                        Err(error) => {
                            self.state = State::Failed;
                            Err(RecordingError::Merge(Some(io::Error::other(error))))
                        }
                    };
                }
                State::Opened | State::Capturing | State::Failed => {
                    if self.store.segments().is_empty() {
                        return Err(RecordingError::NoSegments);
                    }
                    let store = self.store.clone();
                    self.state = State::Finalizing(tokio::task::spawn_blocking(move || {
                        let digest = store.merge()?;
                        store.manifest(digest)
                    }));
                }
            }
        }
    }
}

/// Requires `candidate_dir` to end in `<exam_id>/<oj_user_id>`.
///
/// The runtime owns the layout; recording into a directory that belongs to
/// another candidate would silently misattribute evidence.
fn require_candidate_dir(
    candidate_dir: &Path,
    exam_id: Uuid,
    oj_user_id: i64,
) -> Result<(), RecordingError> {
    let mut components = candidate_dir.components().rev();
    let (Some(user), Some(exam)) = (components.next(), components.next()) else {
        return Err(mismatched(candidate_dir));
    };
    let user_name = oj_user_id.to_string();
    let exam_name = exam_id.to_string();
    if user.as_os_str() == OsStr::new(&user_name) && exam.as_os_str() == OsStr::new(&exam_name) {
        Ok(())
    } else {
        Err(mismatched(candidate_dir))
    }
}

/// Builds the error reported for a directory that belongs to another candidate.
fn mismatched(candidate_dir: &Path) -> RecordingError {
    RecordingError::Io(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "candidate directory {} does not end in <exam_id>/<oj_user_id>",
            candidate_dir.display()
        ),
    ))
}

#[cfg(test)]
mod tests;
