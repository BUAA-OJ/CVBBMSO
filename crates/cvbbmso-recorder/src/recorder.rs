//! Recording lifecycle: opening a candidate, capturing, stopping, and finishing.

mod events;
mod worker;

use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use cvbbmso_protocol::{RecordingPolicy, RecordingSummary};
use tokio::sync::{
    mpsc::{self, Receiver},
    oneshot,
};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{RecorderEvent, RecordingError, store::SegmentStore};
use events::Events;
use worker::{Job, Outcome};

/// Capacity reserved for live events beyond the restored segments.
const EVENT_BUFFER: usize = 16;

/// Recorder of one candidate in one exam.
pub struct Recorder {
    store: SegmentStore,
    policy: RecordingPolicy,
    events: Arc<Events>,
    state: State,
    #[cfg(test)]
    driver: Option<TestDriver>,
}

#[cfg(test)]
type TestDriver = Box<dyn FnOnce(Job, &AtomicBool, oneshot::Sender<()>) -> Outcome + Send>;

/// Lifecycle position of a recorder, as described by the crate document.
enum State {
    /// The directory is open and no capture is running.
    Opened,
    /// Capture is running.
    Capturing(CaptureTask),
    /// The last capture or finish attempt failed; the recorder can retry.
    Failed,
    /// A worker exited without returning the latest sealed-segment state.
    Interrupted,
    /// The recorder owns the merge even if its caller stops awaiting it.
    Finalizing(JoinHandle<Result<RecordingSummary, RecordingError>>),
    /// The merge result stays cached while event delivery completes.
    Complete(RecordingSummary),
}

struct CaptureTask {
    stop: Arc<AtomicBool>,
    task: JoinHandle<Outcome>,
    startup: Startup,
}

enum Startup {
    Pending(oneshot::Receiver<()>),
    Ready,
    Failed,
}

impl Drop for CaptureTask {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
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
    /// when a second recorder owns that directory, when the policy is invalid,
    /// or when an existing segment cannot be read.
    pub async fn open(
        exam_id: Uuid,
        oj_user_id: i64,
        candidate_dir: PathBuf,
        policy: RecordingPolicy,
    ) -> Result<(Self, Receiver<RecorderEvent>), RecordingError> {
        require_candidate_dir(&candidate_dir, exam_id, oj_user_id)?;
        let policy = RecordingPolicy::new(policy.segment_duration_secs, policy.fps)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
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
            policy,
            events: Events::new(events),
            state: State::Opened,
            #[cfg(test)]
            driver: None,
        };
        for meta in restored {
            recorder.events.send(RecorderEvent::SegmentFinalized(meta));
        }
        Ok((recorder, receiver))
    }

    /// Starts or resumes primary-display capture.
    ///
    /// # Errors
    ///
    /// Returns the capture setup error when the desktop cannot be opened or the
    /// recording has already been finalized. Once capture begins, faults are
    /// delivered through [`RecorderEvent::Fault`]. Calling this while capture
    /// runs is idempotent and calling it after a fault starts a new attempt.
    pub async fn start(&mut self) -> Result<(), RecordingError> {
        if let State::Capturing(capture) = &mut self.state {
            if !capture.stop.load(Ordering::Acquire) {
                if matches!(capture.startup, Startup::Pending(_) | Startup::Failed) {
                    return self.await_capture_start().await;
                }
                if !capture.task.is_finished() {
                    return Ok(());
                }
            }
            self.wait_for_capture().await?;
        }
        self.refresh_after_interruption().await?;
        if matches!(self.state, State::Finalizing(_) | State::Complete(_)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a finalizing or completed recording cannot resume",
            )
            .into());
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let (ready, startup) = oneshot::channel();
        let job = Job {
            store: self.store.clone(),
            policy: self.policy,
            events: Arc::clone(&self.events),
        };
        #[cfg(test)]
        let driver = self.driver.take();
        let task = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(driver) = driver {
                return driver(job, &stop_worker, ready);
            }
            job.platform(&stop_worker, ready)
        });
        self.state = State::Capturing(CaptureTask {
            stop,
            task,
            startup: Startup::Pending(startup),
        });
        self.await_capture_start().await
    }

    /// Stops capture and seals the segment being recorded.
    ///
    /// Merging and the summary belong to [`Recorder::finish`]. Stopping an idle
    /// or already stopped recorder does nothing; stopping a failed capture drops
    /// the unsealed segment and keeps the sealed ones.
    ///
    /// # Errors
    ///
    /// Returns the drain or storage error when the pending segment cannot be sealed.
    pub async fn stop(&mut self) -> Result<(), RecordingError> {
        if let State::Capturing(capture) = &self.state {
            capture.stop.store(true, Ordering::Release);
            self.wait_for_capture().await?;
        }
        self.refresh_after_interruption().await?;
        if matches!(self.state, State::Failed) {
            self.state = State::Opened;
        }
        Ok(())
    }

    /// Stops capture, seals the pending segment, merges the recording, and
    /// returns its summary.
    ///
    /// The merge runs on a blocking task because it reads, hashes, and writes
    /// whole segments; awaiting it does not block a Tokio worker.
    /// With an open event receiver, all segment notifications are available to
    /// receive or have already been consumed before success is returned. Drain
    /// the receiver concurrently while waiting when the bounded channel can fill.
    /// Closing or dropping the receiver still allows local finalization.
    ///
    /// Cancelling this future preserves the merge task or its completed result.
    /// The next call resumes waiting. Keep the recorder alive until finishing
    /// completes, as dropping it cannot stop an active filesystem operation.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::NoSegments`] when nothing was recorded and
    /// [`RecordingError::Merge`] when the segments cannot be merged. A failed
    /// attempt leaves the recorder retryable; repeating the call after a
    /// successful finish returns the same summary.
    pub async fn finish(&mut self) -> Result<RecordingSummary, RecordingError> {
        loop {
            match &mut self.state {
                State::Complete(summary) => {
                    self.events.wait_for_delivery().await;
                    return Ok(summary.clone());
                }
                State::Finalizing(task) => {
                    // Await by reference so cancelling the caller cannot detach
                    // the task or allow another writer of the merge temporary
                    // file.
                    match task.await {
                        Ok(Ok(summary)) => self.state = State::Complete(summary),
                        Ok(Err(error)) => {
                            self.state = State::Failed;
                            return Err(error);
                        }
                        Err(error) => {
                            self.state = State::Failed;
                            return Err(RecordingError::Merge(Some(io::Error::other(error))));
                        }
                    }
                }
                State::Capturing(capture) => {
                    capture.stop.store(true, Ordering::Release);
                    self.wait_for_capture().await?;
                }
                State::Interrupted => self.refresh_after_interruption().await?,
                State::Opened | State::Failed => {
                    if self.store.segments().is_empty() {
                        self.state = State::Failed;
                        return Err(RecordingError::NoSegments);
                    }
                    let store = self.store.clone();
                    self.state = State::Finalizing(tokio::task::spawn_blocking(move || {
                        let digest = store.merge()?;
                        store.summary(digest)
                    }));
                }
            }
        }
    }

    async fn await_capture_start(&mut self) -> Result<(), RecordingError> {
        if let State::Capturing(capture) = &mut self.state {
            if let Startup::Pending(ready) = &mut capture.startup {
                capture.startup = if ready.await.is_ok() {
                    Startup::Ready
                } else {
                    Startup::Failed
                };
            }
            if matches!(capture.startup, Startup::Ready) {
                return Ok(());
            }
        }
        self.wait_for_capture().await?;
        Err(RecordingError::Platform(Some(io::Error::new(
            io::ErrorKind::Interrupted,
            "capture stopped before the first accepted frame",
        ))))
    }

    async fn wait_for_capture(&mut self) -> Result<(), RecordingError> {
        let State::Capturing(capture) = &mut self.state else {
            return Ok(());
        };
        let outcome = (&mut capture.task).await;
        self.state = State::Interrupted;
        let outcome = outcome.map_err(io::Error::other)?;
        self.store = outcome.store;
        self.state = if outcome.faulted {
            State::Failed
        } else {
            State::Opened
        };
        outcome.result
    }

    async fn refresh_after_interruption(&mut self) -> Result<(), RecordingError> {
        if matches!(self.state, State::Interrupted) {
            let store = self.store.clone();
            self.store = tokio::task::spawn_blocking(move || store.refresh())
                .await
                .map_err(io::Error::other)??;
            let last_segment = self.events.last_segment();
            for meta in self
                .store
                .segments()
                .into_iter()
                .filter(|meta| meta.index > last_segment)
            {
                self.events.send(RecorderEvent::SegmentFinalized(meta));
            }
            self.state = State::Failed;
        }
        Ok(())
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
