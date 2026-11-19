//! Primary-display capture.

// `Recorder::start` reaches this module once the capture pipeline is wired; the
// platform test exercises it until then.
#![allow(dead_code)]

#[cfg(windows)]
mod windows;

#[cfg(windows)]
#[allow(unused_imports)] // Wired into `Recorder::start` with the capture milestone.
pub(crate) use windows::Desktop;

#[cfg(not(windows))]
#[allow(unused_imports)] // Wired into `Recorder::start` with the capture milestone.
pub(crate) use unsupported::Desktop;

/// One captured frame in BGRA byte order, already in its viewing orientation.
pub(crate) struct Frame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

/// Rotation the desktop applies to the captured surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rotation {
    /// The surface already matches the viewing orientation.
    None,
    /// The surface must be turned 90 degrees clockwise.
    Clockwise90,
    /// The surface must be turned 180 degrees.
    Half,
    /// The surface must be turned 270 degrees clockwise.
    Clockwise270,
}

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

#[cfg(not(windows))]
mod unsupported {
    use super::Frame;
    use crate::RecordingError;

    /// Placeholder backend for platforms without a capture implementation.
    pub(crate) struct Desktop;

    impl Desktop {
        /// # Errors
        ///
        /// Always returns [`RecordingError::Platform`].
        pub(crate) fn open() -> Result<Self, RecordingError> {
            Err(RecordingError::Platform(None))
        }

        /// # Errors
        ///
        /// Always returns [`RecordingError::Platform`].
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
    use super::{Rotation, rotate};

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
}
