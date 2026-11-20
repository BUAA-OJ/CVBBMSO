//! Matroska container support for sealed recording segments.

// The capture and encoding pipeline consumes this module once its milestone
// lands; until then the container is reached only from tests.
#![allow(dead_code)]

mod avc;
mod ebml;
mod mkv;
mod mp4;

pub(crate) use avc::{AvcConfig, with_centered_chroma};
// Annex B parsing and AVCC framing exist only for the Media Foundation encoder,
// which is the sole Windows consumer; other targets never read raw NAL streams.
#[cfg(windows)]
pub(crate) use avc::{annex_b_nals, length_prefixed, nal_type};
pub(crate) use mkv::MkvFile;
#[allow(unused_imports)] // Wired into the capture pipeline with M5.
pub(crate) use mkv::MkvWriter;
pub(crate) use mp4::Mp4Writer;

use thiserror::Error;

/// Colour description shared by the segment and merged containers.
///
/// The values are the Matroska/ISO 14496-12 code points for the BT.709
/// limited-range conversion performed by `Nv12Frame`, with chroma centered in
/// each 2x2 block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Colour {
    /// Colour primaries: 1 is BT.709.
    pub(crate) primaries: u16,
    /// Transfer characteristics: 1 is BT.709.
    pub(crate) transfer: u16,
    /// Matrix coefficients: 1 is BT.709.
    pub(crate) matrix: u16,
    /// Nominal range: 1 is limited (16-235).
    pub(crate) range: u16,
    /// Horizontal chroma siting: 2 is half (centered).
    pub(crate) chroma_siting_horz: u16,
    /// Vertical chroma siting: 2 is half (centered).
    pub(crate) chroma_siting_vert: u16,
}

impl Colour {
    /// The conversion performed by `Nv12Frame`: BT.709 limited, centered chroma.
    pub(crate) const BT709_LIMITED_CENTERED: Self = Self {
        primaries: 1,
        transfer: 1,
        matrix: 1,
        range: 1,
        chroma_siting_horz: 2,
        chroma_siting_vert: 2,
    };
}

/// Failure while writing or reading a recording container.
#[derive(Debug, Error)]
pub(crate) enum ContainerError {
    /// The parameter sets cannot form a decodable configuration record.
    #[error("invalid AVC parameter set")]
    InvalidParameterSet,
    /// The byte stream is not a Matroska file this crate can read.
    #[error("invalid Matroska container")]
    InvalidContainer,
    /// The file relies on a Matroska feature this crate does not implement.
    #[error("unsupported Matroska feature")]
    Unsupported,
    /// Samples were appended out of presentation order.
    #[error("samples are not in presentation order")]
    OutOfOrder,
}

/// One encoded video frame handed to a segment writer.
///
/// This is the seam between the encoder and the container: the encoder fills
/// the fields and the writer only frames them, so neither side needs to know
/// how the other reaches the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sample {
    /// Presentation time in milliseconds, relative to the segment start.
    pub(crate) timestamp_ms: u64,
    /// Whether the frame is a random access point.
    pub(crate) keyframe: bool,
    /// NAL units framed with the four-byte lengths the container stores.
    pub(crate) data: Vec<u8>,
}

impl Sample {
    /// Normalises one H.264 access unit in four-byte length-prefixed form.
    ///
    /// `VideoToolbox` and other platform encoders hand back access units framed
    /// like the container stores them. The in-band SPS is rewritten to the
    /// recorder's centered chroma location so keyframes agree with the
    /// out-of-band `AvcConfig`; an IDR slice marks the sample as a keyframe even
    /// when the platform did not report a sync flag.
    ///
    /// # Errors
    ///
    /// Returns a container error for truncated framing, a malformed parameter
    /// set, or a NAL unit that does not fit the four-byte length.
    pub(crate) fn from_access_unit(
        data: &[u8],
        timestamp_ms: u64,
        keyframe: bool,
    ) -> Result<Option<Self>, ContainerError> {
        let units = avc::length_prefixed_nals(data)?;
        if units.is_empty() {
            return Ok(None);
        }
        let idr = units.iter().any(|unit| avc::nal_type(unit) == 5);
        Ok(Some(Self {
            timestamp_ms,
            keyframe: keyframe || idr,
            data: avc::with_centered_chroma(data)?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::avc::{length_prefixed, with_centered_chroma};
    use super::{ContainerError, Sample};

    /// Microsoft MFT SPS with timing, bitstream restrictions and escaped bytes.
    const SEQUENCE: [u8; 24] = [
        0x67, 0x4D, 0x40, 0x15, 0x95, 0xA0, 0x50, 0x7E, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0x78,
        0x36, 0x82, 0x21, 0x1A, 0x80,
    ];
    const PICTURE: [u8; 4] = [0x68, 0xCE, 0x38, 0x80];
    const IDR: [u8; 3] = [0x65, 0x88, 0x84];

    fn access_unit(units: &[&[u8]]) -> Vec<u8> {
        length_prefixed(units).expect("framing")
    }

    #[test]
    fn idr_marks_the_sample_as_a_keyframe() {
        let units = access_unit(&[&SEQUENCE, &PICTURE, &IDR]);

        let sample = Sample::from_access_unit(&units, 1_500, false)
            .expect("sample")
            .expect("non-empty access unit");

        assert_eq!(sample.timestamp_ms, 1_500);
        assert!(sample.keyframe);
        assert_eq!(sample.data, with_centered_chroma(&units).expect("centered"));
    }

    #[test]
    fn a_non_idr_access_unit_keeps_the_platform_keyframe_flag() {
        let units = access_unit(&[&SEQUENCE, &PICTURE, &[0x41, 0x9A]]);

        assert!(
            !Sample::from_access_unit(&units, 0, false)
                .expect("sample")
                .expect("non-empty access unit")
                .keyframe
        );
        assert!(
            Sample::from_access_unit(&units, 0, true)
                .expect("sample")
                .expect("non-empty access unit")
                .keyframe
        );
    }

    #[test]
    fn an_empty_access_unit_produces_no_sample() {
        assert!(
            Sample::from_access_unit(&[], 0, false)
                .expect("empty")
                .is_none()
        );
    }

    #[test]
    fn truncated_framing_is_rejected() {
        assert!(matches!(
            Sample::from_access_unit(&[0, 0, 0], 0, false),
            Err(ContainerError::InvalidContainer)
        ));
        assert!(matches!(
            Sample::from_access_unit(&[0, 0, 0, 8, 0x65], 0, false),
            Err(ContainerError::InvalidContainer)
        ));
    }
}
