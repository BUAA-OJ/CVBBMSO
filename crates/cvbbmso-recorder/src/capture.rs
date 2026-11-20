//! Primary-display capture.

#[cfg(any(test, target_os = "macos"))]
use std::{
    io,
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

#[cfg(any(test, target_os = "macos"))]
use crate::RecordingError;

mod sampling;

#[cfg(all(test, any(windows, target_os = "macos")))]
pub(crate) use sampling::run;
pub(crate) use sampling::{SampledFrame, run_started};

#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(windows)]
use windows::Desktop;

#[cfg(target_os = "macos")]
use macos::Desktop;

#[cfg(not(any(windows, target_os = "macos")))]
use unsupported::Desktop;

/// One captured frame in BGRA byte order, already in its viewing orientation.
pub(crate) struct Frame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

/// Newest captured image plus the reason capture stopped.
///
/// Push-based backends deliver images from their own callback thread while the
/// sampler pulls them. Keeping only the newest image means a slow consumer can
/// never read a stale frame after a newer one was produced, and a still screen
/// cannot strand the last change behind an unconsumed older image.
#[cfg(any(test, target_os = "macos"))]
pub(crate) struct FrameSlot {
    state: Mutex<SlotState>,
    ready: Condvar,
}

/// Contents of a [`FrameSlot`].
#[derive(Default)]
#[cfg(any(test, target_os = "macos"))]
struct SlotState {
    frame: Option<Frame>,
    stopped: Option<String>,
}

#[cfg(any(test, target_os = "macos"))]
impl FrameSlot {
    /// Creates an empty slot.
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(SlotState::default()),
            ready: Condvar::new(),
        }
    }

    /// Waits up to `timeout` for an image the caller has not consumed yet.
    ///
    /// A pending image is delivered before the stop reason, so a change captured
    /// just before the stream stopped is not lost.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Platform`] with the backend's message once the
    /// stream has stopped; the error repeats for later calls.
    pub(crate) fn next(&self, timeout: Duration) -> Result<Option<Frame>, RecordingError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if let Some(frame) = state.frame.take() {
                return Ok(Some(frame));
            }
            if let Some(message) = &state.stopped {
                return Err(RecordingError::Platform(Some(io::Error::other(
                    message.clone(),
                ))));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let (guard, wait) = self
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            state = guard;
            if wait.timed_out() && state.frame.is_none() && state.stopped.is_none() {
                return Ok(None);
            }
        }
    }

    /// Replaces the pending image, so the newest image always wins.
    pub(crate) fn store(&self, frame: Frame) {
        let mut state = self.lock();
        state.frame = Some(frame);
        drop(state);
        self.ready.notify_all();
    }

    /// Records that capture stopped, waking a waiting consumer.
    pub(crate) fn stop(&self, message: String) {
        let mut state = self.lock();
        state.stopped = Some(message);
        drop(state);
        self.ready.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Rotation the desktop applies to the captured surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(any(windows, test))]
pub(crate) enum Rotation {
    /// The surface already matches the viewing orientation.
    None,
    /// The surface must be turned 90 degrees clockwise.
    Clockwise90,
    /// The surface must be turned 180 degrees.
    Half,
    /// The surface must be turned 270 degrees clockwise.
    #[cfg_attr(not(windows), allow(dead_code))]
    Clockwise270,
}

#[cfg(any(windows, test))]
impl Rotation {
    /// Returns whether the rotation swaps the image axes.
    const fn swaps_axes(self) -> bool {
        matches!(self, Self::Clockwise90 | Self::Clockwise270)
    }
}

/// Rotates a tightly packed BGRA image into its viewing orientation.
///
/// Returns an empty image when `pixels` does not describe `width * height` BGRA
/// pixels.
#[cfg(any(windows, test))]
pub(crate) fn rotate(
    pixels: &[u8],
    width: u32,
    height: u32,
    rotation: Rotation,
) -> (Vec<u8>, u32, u32) {
    const BGRA: usize = 4;

    let columns = usize::try_from(width).unwrap_or(0);
    let rows = usize::try_from(height).unwrap_or(0);
    if columns == 0 || rows == 0 || pixels.len() != columns * rows * BGRA {
        return (Vec::new(), 0, 0);
    }

    let (out_width, out_height) = if rotation.swaps_axes() {
        (height, width)
    } else {
        (width, height)
    };
    let out_columns = usize::try_from(out_width).unwrap_or(0);
    let out_rows = usize::try_from(out_height).unwrap_or(0);

    let mut rotated = vec![0_u8; pixels.len()];
    for y in 0..out_rows {
        for x in 0..out_columns {
            let (source_x, source_y) = match rotation {
                Rotation::None => (x, y),
                Rotation::Half => (columns - 1 - x, rows - 1 - y),
                Rotation::Clockwise90 => (y, rows - 1 - x),
                Rotation::Clockwise270 => (columns - 1 - y, x),
            };
            let source = (source_y * columns + source_x) * BGRA;
            let target = (y * out_columns + x) * BGRA;
            rotated[target..target + BGRA].copy_from_slice(&pixels[source..source + BGRA]);
        }
    }
    (rotated, out_width, out_height)
}

#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported {
    use super::Frame;
    use crate::RecordingError;

    /// Placeholder backend for platforms without a capture implementation.
    pub(crate) struct Desktop;

    impl Desktop {
        /// # Errors
        ///
        /// Always returns [`RecordingError::Platform`].
        pub(crate) fn open(_fps: u32) -> Result<Self, RecordingError> {
            Err(RecordingError::Platform(None))
        }

        /// # Errors
        ///
        /// Always returns [`RecordingError::Platform`].
        // The signature mirrors the Windows backend so `sampling` compiles
        // unchanged; this placeholder has no frame state to read.
        #[allow(clippy::unused_self)]
        pub(crate) fn next_frame(
            &mut self,
            _timeout_ms: u32,
        ) -> Result<Option<Frame>, RecordingError> {
            Err(RecordingError::Platform(None))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Frame, FrameSlot, Rotation, rotate};
    use crate::RecordingError;

    /// A 2x3 image whose pixels encode their own coordinates: `x | y << 4`.
    fn source() -> (Vec<u8>, u32, u32) {
        let mut pixels = Vec::new();
        for y in 0..3_u8 {
            for x in 0..2_u8 {
                pixels.extend_from_slice(&[x, y, 0, 0xFF]);
            }
        }
        (pixels, 2, 3)
    }

    fn pixel_at(pixels: &[u8], width: u32, x: u32, y: u32) -> (u8, u8) {
        let index = usize::try_from((y * width + x) * 4).expect("index");
        (pixels[index], pixels[index + 1])
    }

    #[test]
    fn identity_keeps_dimensions() {
        let (pixels, width, height) = source();

        let (rotated, out_width, out_height) = rotate(&pixels, width, height, Rotation::None);

        assert_eq!((out_width, out_height), (2, 3));
        assert_eq!(rotated, pixels);
    }

    #[test]
    fn quarter_turns_swap_dimensions() {
        let (pixels, width, height) = source();

        let (rotated, out_width, out_height) =
            rotate(&pixels, width, height, Rotation::Clockwise90);

        assert_eq!((out_width, out_height), (3, 2));
        // The bottom-left source pixel becomes the top-left destination pixel.
        assert_eq!(pixel_at(&rotated, out_width, 0, 0), (0, 2));
        assert_eq!(pixel_at(&rotated, out_width, 2, 1), (1, 0));
    }

    #[test]
    fn half_turn_reverses_pixels() {
        let (pixels, width, height) = source();

        let (rotated, out_width, out_height) = rotate(&pixels, width, height, Rotation::Half);

        assert_eq!((out_width, out_height), (2, 3));
        assert_eq!(pixel_at(&rotated, out_width, 0, 0), (1, 2));
        assert_eq!(pixel_at(&rotated, out_width, 1, 2), (0, 0));
    }

    #[test]
    fn mismatched_length_is_rejected() {
        assert_eq!(rotate(&[0, 0, 0], 2, 3, Rotation::None), (Vec::new(), 0, 0));
    }

    /// One-pixel frame tagged by `value` so replacement is visible.
    fn tagged(value: u8) -> Frame {
        Frame {
            width: 1,
            height: 1,
            pixels: vec![value, value, value, value],
        }
    }

    #[test]
    fn a_newer_image_replaces_an_unconsumed_one() {
        let slot = FrameSlot::new();
        slot.store(tagged(1));
        slot.store(tagged(2));

        let frame = slot
            .next(Duration::ZERO)
            .expect("image")
            .expect("pending image");
        assert_eq!(frame.pixels, [2, 2, 2, 2]);
        assert!(slot.next(Duration::ZERO).expect("empty").is_none());
    }

    #[test]
    fn a_stopped_stream_reports_the_reason_after_the_last_image() {
        let slot = FrameSlot::new();
        slot.store(tagged(3));
        slot.stop("screen recording permission was revoked".to_owned());

        assert!(slot.next(Duration::ZERO).expect("last image").is_some());

        let error = slot.next(Duration::ZERO).err().expect("stopped");
        assert!(matches!(error, RecordingError::Platform(Some(_))));
        // The reason repeats instead of degrading into a still screen.
        assert!(slot.next(Duration::ZERO).is_err());
    }
}
