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
        Err(RecordingError::Encoder(None))
    }
}

/// Reusable, top-down NV12 frame with BT.709 limited-range samples.
///
/// Chroma is centered in each 2x2 luma block. Both planes use the same compact
/// stride. This owns the validated dimensions so an undersized or partially
/// converted buffer cannot be passed to the encoder as a successful conversion.
pub(crate) struct Nv12Frame {
    width: usize,
    height: usize,
    bgra_length: usize,
    luma_length: usize,
    bytes: Vec<u8>,
}

impl Nv12Frame {
    /// Creates an even-sized frame initialized to limited-range black.
    ///
    /// # Errors
    ///
    /// Returns a storage error with `InvalidInput` for zero, odd or unrepresentable
    /// dimensions. Allocation failures retain their original source. Odd desktop
    /// dimensions must be padded by the caller before this boundary, never cropped
    /// or silently stripped of their trailing chroma samples.
    pub(crate) fn new(width: u32, height: u32) -> Result<Self, crate::RecordingError> {
        use std::io;

        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(invalid_frame("NV12 requires nonzero even width and height"));
        }
        let width = usize::try_from(width)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let height = usize::try_from(height)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let luma_length = width
            .checked_mul(height)
            .ok_or_else(|| invalid_frame("NV12 image dimensions overflow"))?;
        let bgra_length = luma_length
            .checked_mul(4)
            .filter(|length| isize::try_from(*length).is_ok())
            .ok_or_else(|| invalid_frame("BGRA image size is not representable"))?;
        let length = luma_length
            .checked_add(luma_length / 2)
            .filter(|length| isize::try_from(*length).is_ok())
            .ok_or_else(|| invalid_frame("NV12 image size is not representable"))?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(io::Error::other)?;
        bytes.resize(length, 128);
        bytes[..luma_length].fill(16);
        Ok(Self {
            width,
            height,
            bgra_length,
            luma_length,
            bytes,
        })
    }

    /// Converts exactly `width * height * 4` top-down BGRA bytes in place.
    ///
    /// Alpha is ignored. Y ranges from 16 to 235 and Cb/Cr from 16 to 240.
    /// Coefficients approximate the BT.709 matrix at 16-bit fractional precision.
    /// Each 2x2 block is read once: all four Y samples are written and RGB sums
    /// feed the two chroma samples, with one final rounding after downsampling.
    /// The buffer allocation is reused, including for unchanged desktop images.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for a short or oversized input, leaving this frame
    /// unchanged. This is a compact input contract, not a pitched GPU surface.
    pub(crate) fn convert_bgra(&mut self, pixels: &[u8]) -> Result<(), crate::RecordingError> {
        if pixels.len() != self.bgra_length {
            return Err(invalid_frame(
                "BGRA length must exactly match the frame dimensions",
            ));
        }
        for y in (0..self.height).step_by(2) {
            for x in (0..self.width).step_by(2) {
                let mut red = 0_i32;
                let mut green = 0_i32;
                let mut blue = 0_i32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let position = (y + dy) * self.width + x + dx;
                        let offset = position * 4;
                        let b = i32::from(pixels[offset]);
                        let g = i32::from(pixels[offset + 1]);
                        let r = i32::from(pixels[offset + 2]);
                        let luma = ((11_966 * r + 40_254 * g + 4_064 * b + 32_768) >> 16) + 16;
                        self.bytes[position] = sample_byte(luma, 16, 235);
                        red += r;
                        green += g;
                        blue += b;
                    }
                }
                // Division by four is folded into the final shift, preserving
                // sub-byte RGB averages rather than truncating each channel.
                let u = ((-6_596 * red - 22_188 * green + 28_784 * blue + 131_072) >> 18) + 128;
                let v = ((28_784 * red - 26_145 * green - 2_639 * blue + 131_072) >> 18) + 128;
                let chroma = self.luma_length + (y / 2) * self.width + x;
                self.bytes[chroma] = sample_byte(u, 16, 240);
                self.bytes[chroma + 1] = sample_byte(v, 16, 240);
            }
        }
        Ok(())
    }

    /// Compact Y plane followed by UV pairs, both with stride `width` bytes.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Copies into positive-stride NV12 storage, preserving row padding.
    ///
    /// Y occupies `stride * height` bytes; UV follows at that offset and has
    /// `height / 2` rows with the same stride. The destination must already be
    /// locked by the platform adapter. Padding is outside the visible image.
    /// Returns the storage span including padding for the caller's length field.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for a stride below the visible width, an overflow,
    /// or a short destination. Validation finishes before any bytes are written.
    pub(crate) fn copy_to(
        &self,
        destination: &mut [u8],
        stride: usize,
    ) -> Result<usize, crate::RecordingError> {
        if stride < self.width {
            return Err(invalid_frame(
                "NV12 stride is smaller than the visible width",
            ));
        }
        let rows = self
            .height
            .checked_add(self.height / 2)
            .ok_or_else(|| invalid_frame("NV12 row count overflows"))?;
        let length = stride
            .checked_mul(rows)
            .ok_or_else(|| invalid_frame("NV12 destination span overflows"))?;
        if destination.len() < length {
            return Err(invalid_frame(
                "NV12 destination is too short for its stride",
            ));
        }
        for (source, target) in self
            .bytes
            .chunks_exact(self.width)
            .zip(destination[..length].chunks_exact_mut(stride))
        {
            target[..self.width].copy_from_slice(source);
        }
        Ok(length)
    }
}

/// Clamps a computed sample before narrowing it to the wire's eight bits.
fn sample_byte(value: i32, lower: u8, upper: u8) -> u8 {
    // The clamp proves the value fits u8; negative and high intermediate chroma
    // values cannot escape this boundary.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let byte = value.clamp(i32::from(lower), i32::from(upper)) as u8;
    byte
}

fn invalid_frame(message: &str) -> crate::RecordingError {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.to_owned()).into()
}

#[cfg(test)]
mod tests;
