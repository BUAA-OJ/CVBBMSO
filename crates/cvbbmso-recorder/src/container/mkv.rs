//! Matroska segment writer and reader.

use super::avc::AvcConfig;
use super::ebml::{self, Parser};
use super::{Colour, ContainerError, Sample};

const EBML: u32 = 0x1A45_DFA3;
const EBML_VERSION: u32 = 0x4286;
const EBML_READ_VERSION: u32 = 0x42F7;
const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
const DOC_TYPE: u32 = 0x4282;
const DOC_TYPE_VERSION: u32 = 0x4287;
const DOC_TYPE_READ_VERSION: u32 = 0x4285;

const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TIMECODE_SCALE: u32 = 0x002A_D7B1;
const MUXING_APP: u32 = 0x4D80;
const WRITING_APP: u32 = 0x5741;

const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_UID: u32 = 0x73C5;
const TRACK_TYPE: u32 = 0x83;
const FLAG_LACING: u32 = 0x9C;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const COLOUR: u32 = 0x55B0;
const MATRIX_COEFFICIENTS: u32 = 0x55B1;
const CHROMA_SITING_HORZ: u32 = 0x55B7;
const CHROMA_SITING_VERT: u32 = 0x55B8;
const RANGE: u32 = 0x55B9;
const TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
const PRIMARIES: u32 = 0x55BB;

const CLUSTER: u32 = 0x1F43_B675;
const TIMECODE: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;

const TRACK_TYPE_VIDEO: u64 = 1;
const TRACK_NUMBER_BYTE: u8 = 0x81;
const KEYFRAME_FLAG: u8 = 0x80;
const CODEC_ID_AVC: &str = "V_MPEG4/ISO/AVC";
const APP: &str = "cvbbmso";

/// Timecode scale written into every segment, in nanoseconds per unit.
const TIMECODE_SCALE_NS: u64 = 1_000_000;
/// Longest span covered by one cluster, in milliseconds.
///
/// A block stores its offset from the cluster timecode as a signed 16-bit
/// value, so clusters must stay well below 32 seconds.
const MAX_CLUSTER_SPAN_MS: u64 = 30_000;

/// Builds one Matroska segment file.
///
/// Samples are collected in memory and rendered in one pass. A segment is
/// bounded by the recording policy, so the buffer stays proportional to a single
/// segment instead of the whole recording.
pub(crate) struct MkvWriter {
    config: AvcConfig,
    width: u32,
    height: u32,
    samples: Vec<Sample>,
}

impl MkvWriter {
    /// Creates a writer for one video track.
    pub(crate) fn new(config: AvcConfig, width: u32, height: u32) -> Self {
        Self {
            config,
            width,
            height,
            samples: Vec::new(),
        }
    }

    /// Appends one frame.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::OutOfOrder`] when the frame precedes the
    /// previously appended one.
    pub(crate) fn push(&mut self, sample: Sample) -> Result<(), ContainerError> {
        if self
            .samples
            .last()
            .is_some_and(|previous| sample.timestamp_ms < previous.timestamp_ms)
        {
            return Err(ContainerError::OutOfOrder);
        }
        self.samples.push(sample);
        Ok(())
    }

    /// Renders the complete file.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::InvalidParameterSet`] when the configuration
    /// record cannot be built, and [`ContainerError::Unsupported`] when a block
    /// offset exceeds the signed 16-bit field.
    pub(crate) fn finish(self) -> Result<Vec<u8>, ContainerError> {
        let mut file = Vec::new();
        write_header(&mut file);

        let mut body = Vec::new();
        write_info(&mut body);
        write_tracks(&mut body, &self.config, self.width, self.height)?;
        write_clusters(&mut body, &self.samples)?;
        ebml::element(&mut file, SEGMENT, &body);
        Ok(file)
    }
}

/// A parsed Matroska segment file.
pub(crate) struct MkvFile {
    pub(crate) config: AvcConfig,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) colour: Option<Colour>,
    pub(crate) samples: Vec<Sample>,
}

impl MkvFile {
    /// Parses a segment file produced by [`MkvWriter`].
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::InvalidContainer`] when the file is not a
    /// single-track Matroska video segment, and
    /// [`ContainerError::InvalidParameterSet`] or [`ContainerError::Unsupported`]
    /// when the codec private data cannot be decoded.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, ContainerError> {
        let mut parser = Parser::new(bytes);
        let (id, _) = parser
            .next_element()?
            .ok_or(ContainerError::InvalidContainer)?;
        if id != EBML {
            return Err(ContainerError::InvalidContainer);
        }
        let (id, segment) = parser
            .next_element()?
            .ok_or(ContainerError::InvalidContainer)?;
        if id != SEGMENT {
            return Err(ContainerError::InvalidContainer);
        }

        let mut timecode_scale = TIMECODE_SCALE_NS;
        let mut track = None;
        let mut samples = Vec::new();
        let mut parser = Parser::new(segment);
        while let Some((id, payload)) = parser.next_element()? {
            match id {
                INFO => timecode_scale = parse_info(payload)?,
                TRACKS => track = Some(parse_tracks(payload)?),
                CLUSTER => parse_cluster(payload, timecode_scale, &mut samples)?,
                _ => {}
            }
        }

        let (config, width, height, colour) = track.ok_or(ContainerError::InvalidContainer)?;
        Ok(Self {
            config,
            width,
            height,
            colour,
            samples,
        })
    }
}

fn write_header(out: &mut Vec<u8>) {
    let mut header = Vec::new();
    ebml::unsigned(&mut header, EBML_VERSION, 1);
    ebml::unsigned(&mut header, EBML_READ_VERSION, 1);
    ebml::unsigned(&mut header, EBML_MAX_ID_LENGTH, 4);
    ebml::unsigned(&mut header, EBML_MAX_SIZE_LENGTH, 8);
    ebml::text(&mut header, DOC_TYPE, "matroska");
    ebml::unsigned(&mut header, DOC_TYPE_VERSION, 4);
    ebml::unsigned(&mut header, DOC_TYPE_READ_VERSION, 2);
    ebml::element(out, EBML, &header);
}

/// Writes the segment information.
///
/// The element carries no duration: the sealed metadata of the segment already
/// records the authoritative start and end times, and the writer does not know
/// the frame cadence the encoder used.
fn write_info(out: &mut Vec<u8>) {
    let mut info = Vec::new();
    ebml::unsigned(&mut info, TIMECODE_SCALE, TIMECODE_SCALE_NS);
    ebml::text(&mut info, MUXING_APP, APP);
    ebml::text(&mut info, WRITING_APP, APP);
    ebml::element(out, INFO, &info);
}

fn write_tracks(
    out: &mut Vec<u8>,
    config: &AvcConfig,
    width: u32,
    height: u32,
) -> Result<(), ContainerError> {
    let mut video = Vec::new();
    ebml::unsigned(&mut video, PIXEL_WIDTH, u64::from(width));
    ebml::unsigned(&mut video, PIXEL_HEIGHT, u64::from(height));
    let mut colour = Vec::new();
    ebml::unsigned(
        &mut colour,
        MATRIX_COEFFICIENTS,
        u64::from(Colour::BT709_LIMITED_CENTERED.matrix),
    );
    ebml::unsigned(
        &mut colour,
        CHROMA_SITING_HORZ,
        u64::from(Colour::BT709_LIMITED_CENTERED.chroma_siting_horz),
    );
    ebml::unsigned(
        &mut colour,
        CHROMA_SITING_VERT,
        u64::from(Colour::BT709_LIMITED_CENTERED.chroma_siting_vert),
    );
    ebml::unsigned(
        &mut colour,
        RANGE,
        u64::from(Colour::BT709_LIMITED_CENTERED.range),
    );
    ebml::unsigned(
        &mut colour,
        TRANSFER_CHARACTERISTICS,
        u64::from(Colour::BT709_LIMITED_CENTERED.transfer),
    );
    ebml::unsigned(
        &mut colour,
        PRIMARIES,
        u64::from(Colour::BT709_LIMITED_CENTERED.primaries),
    );
    ebml::element(&mut video, COLOUR, &colour);

    let mut entry = Vec::new();
    ebml::unsigned(&mut entry, TRACK_NUMBER, 1);
    ebml::unsigned(&mut entry, TRACK_UID, 1);
    ebml::unsigned(&mut entry, TRACK_TYPE, TRACK_TYPE_VIDEO);
    ebml::unsigned(&mut entry, FLAG_LACING, 0);
    ebml::text(&mut entry, CODEC_ID, CODEC_ID_AVC);
    ebml::element(&mut entry, CODEC_PRIVATE, &config.record()?);
    ebml::element(&mut entry, VIDEO, &video);

    let mut tracks = Vec::new();
    ebml::element(&mut tracks, TRACK_ENTRY, &entry);
    ebml::element(out, TRACKS, &tracks);
    Ok(())
}

fn write_clusters(out: &mut Vec<u8>, samples: &[Sample]) -> Result<(), ContainerError> {
    let mut blocks = Vec::new();
    let mut base = 0_u64;
    for sample in samples {
        if blocks.is_empty() {
            base = sample.timestamp_ms;
        } else if sample.timestamp_ms.saturating_sub(base) > MAX_CLUSTER_SPAN_MS {
            write_cluster(out, base, &blocks);
            blocks.clear();
            base = sample.timestamp_ms;
        }
        write_block(
            &mut blocks,
            sample.timestamp_ms.saturating_sub(base),
            sample,
        )?;
    }
    if !blocks.is_empty() {
        write_cluster(out, base, &blocks);
    }
    Ok(())
}

fn write_cluster(out: &mut Vec<u8>, base: u64, blocks: &[u8]) {
    let mut cluster = Vec::with_capacity(blocks.len() + 8);
    ebml::unsigned(&mut cluster, TIMECODE, base);
    cluster.extend_from_slice(blocks);
    ebml::element(out, CLUSTER, &cluster);
}

fn write_block(out: &mut Vec<u8>, relative_ms: u64, sample: &Sample) -> Result<(), ContainerError> {
    let relative = i16::try_from(relative_ms).map_err(|_| ContainerError::Unsupported)?;
    let mut block = Vec::with_capacity(sample.data.len() + 4);
    block.push(TRACK_NUMBER_BYTE);
    block.extend_from_slice(&relative.to_be_bytes());
    block.push(if sample.keyframe { KEYFRAME_FLAG } else { 0 });
    block.extend_from_slice(&sample.data);
    ebml::element(out, SIMPLE_BLOCK, &block);
    Ok(())
}

fn parse_info(payload: &[u8]) -> Result<u64, ContainerError> {
    let mut parser = Parser::new(payload);
    let mut scale = TIMECODE_SCALE_NS;
    while let Some((id, value)) = parser.next_element()? {
        if id == TIMECODE_SCALE {
            scale = Parser::unsigned(value)?;
        }
    }
    Ok(scale)
}

fn parse_tracks(payload: &[u8]) -> Result<(AvcConfig, u32, u32, Option<Colour>), ContainerError> {
    let mut parser = Parser::new(payload);
    while let Some((id, entry)) = parser.next_element()? {
        if id != TRACK_ENTRY {
            continue;
        }
        let mut config = None;
        let mut width = 0;
        let mut height = 0;
        let mut colour = None;
        let mut fields = Parser::new(entry);
        while let Some((id, value)) = fields.next_element()? {
            match id {
                CODEC_PRIVATE => config = Some(AvcConfig::parse(value)?),
                VIDEO => (width, height, colour) = parse_video(value)?,
                _ => {}
            }
        }
        let config = config.ok_or(ContainerError::InvalidContainer)?;
        return Ok((config, width, height, colour));
    }
    Err(ContainerError::InvalidContainer)
}

fn parse_video(payload: &[u8]) -> Result<(u32, u32, Option<Colour>), ContainerError> {
    let mut parser = Parser::new(payload);
    let mut width = 0;
    let mut height = 0;
    let mut colour = None;
    while let Some((id, value)) = parser.next_element()? {
        match id {
            PIXEL_WIDTH => {
                width = u32::try_from(Parser::unsigned(value)?)
                    .map_err(|_| ContainerError::InvalidContainer)?;
            }
            PIXEL_HEIGHT => {
                height = u32::try_from(Parser::unsigned(value)?)
                    .map_err(|_| ContainerError::InvalidContainer)?;
            }
            COLOUR => colour = Some(parse_colour(value)?),
            _ => {}
        }
    }
    Ok((width, height, colour))
}

fn parse_colour(payload: &[u8]) -> Result<Colour, ContainerError> {
    let mut parser = Parser::new(payload);
    let mut colour = Colour {
        primaries: 0,
        transfer: 0,
        matrix: 0,
        range: 0,
        chroma_siting_horz: 0,
        chroma_siting_vert: 0,
    };
    while let Some((id, value)) = parser.next_element()? {
        let number = || {
            u16::try_from(Parser::unsigned(value)?).map_err(|_| ContainerError::InvalidContainer)
        };
        match id {
            PRIMARIES => colour.primaries = number()?,
            TRANSFER_CHARACTERISTICS => colour.transfer = number()?,
            MATRIX_COEFFICIENTS => colour.matrix = number()?,
            RANGE => colour.range = number()?,
            CHROMA_SITING_HORZ => colour.chroma_siting_horz = number()?,
            CHROMA_SITING_VERT => colour.chroma_siting_vert = number()?,
            _ => {}
        }
    }
    Ok(colour)
}

fn parse_cluster(
    payload: &[u8],
    scale: u64,
    samples: &mut Vec<Sample>,
) -> Result<(), ContainerError> {
    let mut parser = Parser::new(payload);
    let mut timecode = 0_i64;
    while let Some((id, value)) = parser.next_element()? {
        match id {
            TIMECODE => {
                timecode = i64::try_from(Parser::unsigned(value)?)
                    .map_err(|_| ContainerError::InvalidContainer)?;
            }
            SIMPLE_BLOCK => samples.push(parse_block(value, timecode, scale)?),
            _ => {}
        }
    }
    Ok(())
}

fn parse_block(
    payload: &[u8],
    cluster_timecode: i64,
    scale: u64,
) -> Result<Sample, ContainerError> {
    let first = *payload.first().ok_or(ContainerError::InvalidContainer)?;
    let track_length =
        usize::try_from(first.leading_zeros()).map_err(|_| ContainerError::InvalidContainer)? + 1;

    let mut offset = track_length;
    let relative = payload
        .get(offset..offset + 2)
        .ok_or(ContainerError::InvalidContainer)?;
    let relative = i16::from_be_bytes([relative[0], relative[1]]);
    offset += 2;
    let flags = *payload
        .get(offset)
        .ok_or(ContainerError::InvalidContainer)?;
    offset += 1;
    let data = payload
        .get(offset..)
        .ok_or(ContainerError::InvalidContainer)?
        .to_vec();

    let raw = cluster_timecode
        .checked_add(i64::from(relative))
        .ok_or(ContainerError::InvalidContainer)?;
    let scaled = i128::from(raw) * i128::from(scale) / i128::from(TIMECODE_SCALE_NS);
    let timestamp_ms = u64::try_from(scaled).map_err(|_| ContainerError::InvalidContainer)?;
    Ok(Sample {
        timestamp_ms,
        keyframe: flags & KEYFRAME_FLAG != 0,
        data,
    })
}

#[cfg(test)]
mod tests;
