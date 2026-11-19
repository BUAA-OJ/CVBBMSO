//! H.264 encoding of captured frames.

// `Recorder::start` reaches this module once the capture pipeline is wired; the
// platform probe exercises it until then.
#![allow(dead_code)]

#[cfg(windows)]
mod windows;

#[cfg(windows)]
#[allow(unused_imports)] // Wired into `Recorder::start` with the encoding milestone.
pub(crate) use windows::available_encoders;

#[cfg(not(windows))]
#[allow(unused_imports)] // Wired into `Recorder::start` with the encoding milestone.
pub(crate) use unsupported::available_encoders;

#[cfg(not(windows))]
mod unsupported {
    use crate::RecordingError;

    /// Reports the encoders Media Foundation offers for the recording policy.
    ///
    /// # Errors
    ///
    /// Always returns [`RecordingError::Encoder`]: this crate has no encoder for
    /// the current platform.
    pub(crate) fn available_encoders() -> Result<Vec<String>, RecordingError> {
        Err(RecordingError::Encoder)
    }
}

/// Bytes per pixel of a captured BGRA frame.
const BGRA: usize = 4;
/// Colour-difference samples stored per chroma block.
const CHROMA_SAMPLES: usize = 2;

/// Converts a tightly packed BGRA frame into the NV12 layout an encoder expects.
///
/// The conversion uses the BT.601 limited-range matrix, which is what H.264
/// encoders assume for the chroma samples in NV12: luma spans 16–235 and chroma
/// 16–240 rather than the full 0–255 range.
///
/// The result is the luma plane (`width * height` bytes) followed by the
/// interleaved chroma plane (`width / 2 * height / 2` Cb/Cr pairs). Odd
/// dimensions are accepted; the trailing row and column do not contribute chroma.
/// Returns an empty buffer when `pixels` does not describe `width * height` BGRA
/// pixels.
pub(crate) fn bgra_to_nv12(pixels: &[u8], width: u32, height: u32) -> Vec<u8> {
    let columns = usize::try_from(width).unwrap_or(0);
    let rows = usize::try_from(height).unwrap_or(0);
    if columns == 0 || rows == 0 || pixels.len() < columns * rows * BGRA {
        return Vec::new();
    }

    let luma_length = columns * rows;
    let chroma_columns = columns / 2;
    let chroma_rows = rows / 2;
    let mut nv12 = vec![0_u8; luma_length + chroma_columns * chroma_rows * CHROMA_SAMPLES];

    for y in 0..rows {
        for x in 0..columns {
            let offset = (y * columns + x) * BGRA;
            let blue = i32::from(pixels[offset]);
            let green = i32::from(pixels[offset + 1]);
            let red = i32::from(pixels[offset + 2]);
            let luma = ((66 * red + 129 * green + 25 * blue + 128) >> 8) + 16;
            nv12[y * columns + x] = u8::try_from(luma.clamp(0, 255)).unwrap_or(0);
        }
    }

    for y in 0..chroma_rows {
        for x in 0..chroma_columns {
            let mut red = 0_i32;
            let mut green = 0_i32;
            let mut blue = 0_i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let offset = ((y * 2 + dy) * columns + (x * 2 + dx)) * BGRA;
                    blue += i32::from(pixels[offset]);
                    green += i32::from(pixels[offset + 1]);
                    red += i32::from(pixels[offset + 2]);
                }
            }
            // Average the block before applying the matrix so the division folds
            // into the shift.
            let red = red / 4;
            let green = green / 4;
            let blue = blue / 4;
            let u = ((-38 * red - 74 * green + 112 * blue + 128) >> 8) + 128;
            let v = ((112 * red - 94 * green - 18 * blue + 128) >> 8) + 128;
            let chroma = luma_length + (y * chroma_columns + x) * CHROMA_SAMPLES;
            nv12[chroma] = u8::try_from(u.clamp(0, 255)).unwrap_or(0);
            nv12[chroma + 1] = u8::try_from(v.clamp(0, 255)).unwrap_or(0);
        }
    }
    nv12
}

#[cfg(test)]
mod tests;
