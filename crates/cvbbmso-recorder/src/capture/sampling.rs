//! Frame sampling on the thread that owns the platform capture backend.

use std::{
    io,
    num::NonZeroU32,
    ops::ControlFlow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use cvbbmso_protocol::RecordingPolicy;

use super::{Desktop, Frame};
use crate::RecordingError;

/// Bound on an idle wait before checking the stop request again.
const STOP_POLL: Duration = Duration::from_millis(50);
const STOP_POLL_MS: u32 = 50;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// One sampled image and its monotonic presentation time since capture started.
pub(crate) struct SampledFrame {
    pub(crate) elapsed: Duration,
    // Still screens share their buffer; they do not allocate a new image per tick.
    pub(crate) frame: Arc<Frame>,
}

/// Captures at most one image per policy sampling slot until stopped.
///
/// This is a blocking thread entry point: callers must run it on a dedicated
/// thread. It creates and drops the platform backend on that same thread. The
/// sink runs synchronously, so it must finish in bounded time; no frame queue is
/// allocated here. A slow sink skips elapsed sampling slots rather than causing
/// a burst of catch-up frames. `Break(())` ends capture normally.
///
/// Before the first image, timeouts produce no sample. Afterwards, timeouts reuse
/// the last image with a new presentation time. Times are monotonic durations,
/// independent of wall-clock adjustments; segment UTC times belong to sealing.
///
/// # Errors
///
/// Rejects invalid policies before opening the backend. Returns platform capture
/// failures and sink failures unchanged. The caller handles reporting the fault.
#[cfg(test)]
pub(crate) fn run(
    policy: RecordingPolicy,
    stopped: &AtomicBool,
    sink: impl FnMut(SampledFrame) -> Result<ControlFlow<()>, RecordingError>,
) -> Result<(), RecordingError> {
    run_started(policy, stopped, || {}, sink)
}

/// Runs the sampler, announcing readiness after the first frame reaches the sink.
pub(crate) fn run_started(
    policy: RecordingPolicy,
    stopped: &AtomicBool,
    started: impl FnOnce(),
    sink: impl FnMut(SampledFrame) -> Result<ControlFlow<()>, RecordingError>,
) -> Result<(), RecordingError> {
    RecordingPolicy::new(policy.segment_duration_secs, policy.fps)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let fps = NonZeroU32::new(policy.fps)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "zero capture frame rate"))?;
    if stopped.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut desktop = Desktop::open(fps.get())?;
    let mut clock = MonotonicClock(Instant::now());
    sample_started(&mut desktop, &mut clock, fps, stopped, started, sink)
}

fn sample_started(
    source: &mut impl FrameSource,
    clock: &mut impl Clock,
    fps: NonZeroU32,
    stopped: &AtomicBool,
    started: impl FnOnce(),
    mut sink: impl FnMut(SampledFrame) -> Result<ControlFlow<()>, RecordingError>,
) -> Result<(), RecordingError> {
    let mut started = Some(started);
    sample(source, clock, fps, stopped, |frame| {
        let result = sink(frame)?;
        if let Some(started) = started.take() {
            started();
        }
        Ok(result)
    })
}

/// Platform boundary and deterministic test seam for changed images.
trait FrameSource {
    fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<Frame>, RecordingError>;
}

impl FrameSource for Desktop {
    fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<Frame>, RecordingError> {
        Self::next_frame(self, timeout_ms)
    }
}

/// Clock and waiting seam; tests advance time without sleeping.
trait Clock {
    fn now(&self) -> Duration;
    fn wait_until(&mut self, deadline: Duration, stopped: &AtomicBool);
}

struct MonotonicClock(Instant);

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }

    fn wait_until(&mut self, deadline: Duration, stopped: &AtomicBool) {
        while !stopped.load(Ordering::Acquire) {
            let remaining = deadline.saturating_sub(self.now());
            if remaining.is_zero() {
                return;
            }
            thread::sleep(remaining.min(STOP_POLL));
        }
    }
}

fn sample(
    source: &mut impl FrameSource,
    clock: &mut impl Clock,
    fps: NonZeroU32,
    stopped: &AtomicBool,
    mut sink: impl FnMut(SampledFrame) -> Result<ControlFlow<()>, RecordingError>,
) -> Result<(), RecordingError> {
    let mut latest = None;
    let mut deadline = Duration::ZERO;
    loop {
        clock.wait_until(deadline, stopped);
        if stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        // With a cached image, poll once at the sampling point. Waiting for an
        // update would delay a still image's presentation time unnecessarily.
        let timeout_ms = if latest.is_some() { 0 } else { STOP_POLL_MS };
        // GPU readback can take time. Timestamp the sampling attempt, not the
        // completion of its buffer copy or the consumer's work.
        let sampled_at = clock.now();
        let frame = source.next_frame(timeout_ms)?;
        if stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        if latest.is_none() && clock.now() >= FIRST_FRAME_TIMEOUT {
            return Err(RecordingError::Platform(Some(io::Error::new(
                io::ErrorKind::TimedOut,
                "no desktop image received within five seconds",
            ))));
        }
        if let Some(frame) = frame {
            latest = Some(Arc::new(frame));
        }
        if let Some(frame) = &latest {
            let sample = SampledFrame {
                elapsed: sampled_at,
                frame: Arc::clone(frame),
            };
            if sink(sample)?.is_break() {
                return Ok(());
            }
        }
        deadline = next_deadline(clock.now(), fps)?;
    }
}

/// First sampling point strictly after `now`, rounded up to a nanosecond.
///
/// Absolute rational ticks avoid cumulative rounding drift (for example 15 fps),
/// and also stay nonzero for policies above one billion frames per second.
fn next_deadline(now: Duration, fps: NonZeroU32) -> Result<Duration, RecordingError> {
    let rate = u128::from(fps.get());
    // Even Duration::MAX * u32::MAX fits u128 when expressed in nanoseconds.
    let tick = now.as_nanos() * rate / NANOS_PER_SECOND + 1;
    let nanos = (tick * NANOS_PER_SECOND).div_ceil(rate);
    let seconds = u64::try_from(nanos / NANOS_PER_SECOND).map_err(io::Error::other)?;
    let remainder = u32::try_from(nanos % NANOS_PER_SECOND).map_err(io::Error::other)?;
    Ok(Duration::new(seconds, remainder))
}

#[cfg(test)]
mod tests;
