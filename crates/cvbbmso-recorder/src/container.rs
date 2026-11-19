//! Matroska container support for sealed recording segments.

// The capture and encoding pipeline consumes this module once its milestone
// lands; until then the container is reached only from tests.
#![allow(dead_code)]

mod avc;
mod ebml;
mod mkv;
mod mp4;

pub(crate) use avc::{AvcConfig, annex_b_nals, length_prefixed, nal_type};
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
