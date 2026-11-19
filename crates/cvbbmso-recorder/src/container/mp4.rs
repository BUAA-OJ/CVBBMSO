//! MP4 writer for the merged recording.
//!
//! The merged file is a single H.264 track with one sample per encoded frame.
//! The caller writes the sample payloads after [`Mp4Writer::start`] returns; the
//! box layout and chunk offset are fixed before the first payload byte.

use super::avc::AvcConfig;
use super::{Colour, ContainerError};

/// Movie timescale: one tick per millisecond.
const TIMESCALE: u32 = 1_000;
/// Identity transform matrix shared by the movie and track headers.
const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
/// Language code for `und`, packed as ISO-639-2/T.
const LANGUAGE_UND: u16 = 0x55C4;
/// Compression name stored in the `avc1` sample entry.
const COMPRESSOR: &str = "cvbbmso";

/// Video layout shared by every merged segment.
struct Layout<'a> {
    config: &'a AvcConfig,
    width: u32,
    height: u32,
}

/// One frame's container metadata.
struct Entry {
    timestamp_ms: u64,
    keyframe: bool,
    size: u32,
}

/// Builds the header of one merged recording.
pub(crate) struct Mp4Writer {
    config: AvcConfig,
    width: u32,
    height: u32,
    entries: Vec<Entry>,
    data_length: u64,
    tail_duration: Option<u32>,
}

impl Mp4Writer {
    /// Creates a writer for one video track.
    pub(crate) fn new(config: AvcConfig, width: u32, height: u32) -> Self {
        Self {
            config,
            width,
            height,
            entries: Vec::new(),
            data_length: 0,
            tail_duration: None,
        }
    }

    /// Overrides the duration of the last sample.
    ///
    /// The merge step knows how long the final frame was shown; without an
    /// explicit value the writer would reuse the previous sample's delta.
    pub(crate) fn set_tail_duration(&mut self, duration_ms: u32) {
        self.tail_duration = Some(duration_ms.max(1));
    }

    /// Appends one sample's metadata.
    ///
    /// The payload itself is written by the caller after [`Self::start`], in the
    /// same order.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::OutOfOrder`] when the timestamp precedes the
    /// previous sample, and [`ContainerError::Unsupported`] when the sample
    /// count or size does not fit the MP4 tables.
    pub(crate) fn push(
        &mut self,
        timestamp_ms: u64,
        keyframe: bool,
        size: usize,
    ) -> Result<(), ContainerError> {
        if self
            .entries
            .last()
            .is_some_and(|previous| timestamp_ms < previous.timestamp_ms)
        {
            return Err(ContainerError::OutOfOrder);
        }
        let size = u32::try_from(size).map_err(|_| ContainerError::Unsupported)?;
        self.entries.push(Entry {
            timestamp_ms,
            keyframe,
            size,
        });
        self.data_length = self
            .data_length
            .checked_add(u64::from(size))
            .ok_or(ContainerError::Unsupported)?;
        Ok(())
    }

    /// Total number of sample payload bytes that must follow [`Self::start`].
    pub(crate) const fn data_length(&self) -> u64 {
        self.data_length
    }

    /// Renders `ftyp`, `moov`, and the `mdat` header.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::InvalidParameterSet`] when the AVC configuration
    /// cannot be encoded, and [`ContainerError::Unsupported`] when a dimension or
    /// count does not fit its MP4 field.
    pub(crate) fn start(&self) -> Result<Vec<u8>, ContainerError> {
        let file_type = file_type();
        let header_length = mdat_header_length(self.data_length);
        let mut wide = false;
        loop {
            let estimate = self.movie(0, wide)?;
            let offset = u64::try_from(file_type.len() + estimate.len() + header_length)
                .map_err(|_| ContainerError::Unsupported)?;
            let total = offset
                .checked_add(self.data_length)
                .ok_or(ContainerError::Unsupported)?;
            if !wide && total > u64::from(u32::MAX) {
                wide = true;
                continue;
            }
            let movie = self.movie(offset, wide)?;
            let mut file = Vec::with_capacity(file_type.len() + movie.len() + header_length);
            file.extend_from_slice(&file_type);
            file.extend_from_slice(&movie);
            write_mdat_header(&mut file, self.data_length);
            return Ok(file);
        }
    }

    /// Renders the complete `moov` box with the given first-chunk offset.
    fn movie(&self, chunk_offset: u64, wide: bool) -> Result<Vec<u8>, ContainerError> {
        let deltas = self.deltas()?;
        let duration: u32 = deltas
            .iter()
            .try_fold(0_u64, |total, delta| total.checked_add(u64::from(*delta)))
            .and_then(|total| u32::try_from(total).ok())
            .ok_or(ContainerError::Unsupported)?;

        let mut movie = Vec::new();
        let layout = Layout {
            config: &self.config,
            width: self.width,
            height: self.height,
        };
        movie.extend_from_slice(&movie_header(duration));
        movie.extend_from_slice(&track(
            &layout,
            duration,
            &self.entries,
            &deltas,
            chunk_offset,
            wide,
        )?);
        Ok(boxed(*b"moov", &movie))
    }

    /// Per-sample durations in the movie timescale.
    fn deltas(&self) -> Result<Vec<u32>, ContainerError> {
        let mut deltas = Vec::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            let last_duration = self
                .tail_duration
                .or_else(|| deltas.last().copied())
                .unwrap_or(1);
            let delta = self
                .entries
                .get(index + 1)
                .map_or(u64::from(last_duration), |next| {
                    next.timestamp_ms.saturating_sub(entry.timestamp_ms)
                })
                .max(1);
            deltas.push(u32::try_from(delta).map_err(|_| ContainerError::Unsupported)?);
        }
        Ok(deltas)
    }
}

/// Renders the file type box.
fn file_type() -> Vec<u8> {
    let mut payload = Vec::with_capacity(24);
    payload.extend_from_slice(b"isom");
    push_u32(&mut payload, 512);
    for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
        payload.extend_from_slice(brand);
    }
    boxed(*b"ftyp", &payload)
}

/// Renders the movie header.
fn movie_header(duration: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(100);
    // creation and modification time are zero: the merged file describes media
    // order, and the authoritative wall-clock times live in the segment metadata.
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, TIMESCALE);
    push_u32(&mut payload, duration);
    push_u32(&mut payload, 0x0001_0000); // rate 1.0
    push_u16(&mut payload, 0x0100); // volume 1.0
    payload.extend_from_slice(&[0; 10]);
    for value in MATRIX {
        push_u32(&mut payload, value);
    }
    payload.extend_from_slice(&[0; 24]);
    push_u32(&mut payload, 2); // next track ID
    full_box(*b"mvhd", 0, 0, &payload)
}

/// Renders the single video track.
fn track(
    layout: &Layout<'_>,
    duration: u32,
    entries: &[Entry],
    deltas: &[u32],
    chunk_offset: u64,
    wide: bool,
) -> Result<Vec<u8>, ContainerError> {
    let mut track = Vec::new();
    track.extend_from_slice(&track_header(layout.width, layout.height, duration)?);
    track.extend_from_slice(&media_box(
        layout,
        duration,
        entries,
        deltas,
        chunk_offset,
        wide,
    )?);
    Ok(boxed(*b"trak", &track))
}

/// Renders the track header.
fn track_header(width: u32, height: u32, duration: u32) -> Result<Vec<u8>, ContainerError> {
    let width = u16::try_from(width).map_err(|_| ContainerError::Unsupported)?;
    let height = u16::try_from(height).map_err(|_| ContainerError::Unsupported)?;
    let mut payload = Vec::with_capacity(84);
    push_u32(&mut payload, 0); // creation time
    push_u32(&mut payload, 0); // modification time
    push_u32(&mut payload, 1); // track ID
    push_u32(&mut payload, 0); // reserved
    push_u32(&mut payload, duration);
    payload.extend_from_slice(&[0; 8]); // reserved
    push_u16(&mut payload, 0); // layer
    push_u16(&mut payload, 0); // alternate group
    push_u16(&mut payload, 0); // volume
    push_u16(&mut payload, 0); // reserved
    for value in MATRIX {
        push_u32(&mut payload, value);
    }
    push_u32(&mut payload, u32::from(width) << 16);
    push_u32(&mut payload, u32::from(height) << 16);
    Ok(full_box(*b"tkhd", 0, 0x0007, &payload))
}

/// Renders `mdia` and its `minf`/`stbl` descendants.
fn media_box(
    layout: &Layout<'_>,
    duration: u32,
    entries: &[Entry],
    deltas: &[u32],
    chunk_offset: u64,
    wide: bool,
) -> Result<Vec<u8>, ContainerError> {
    let mut media = Vec::new();
    media.extend_from_slice(&media_header(duration));
    media.extend_from_slice(&handler());
    media.extend_from_slice(&media_information(
        layout,
        entries,
        deltas,
        chunk_offset,
        wide,
    )?);
    Ok(boxed(*b"mdia", &media))
}

/// Renders the media header.
fn media_header(duration: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(20);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, TIMESCALE);
    push_u32(&mut payload, duration);
    push_u16(&mut payload, LANGUAGE_UND);
    push_u16(&mut payload, 0);
    full_box(*b"mdhd", 0, 0, &payload)
}

/// Renders the video handler.
fn handler() -> Vec<u8> {
    let mut payload = Vec::with_capacity(25);
    push_u32(&mut payload, 0);
    payload.extend_from_slice(b"vide");
    payload.extend_from_slice(&[0; 12]);
    payload.extend_from_slice(b"VideoHandler\0");
    full_box(*b"hdlr", 0, 0, &payload)
}

/// Renders `minf` with video, data reference, and sample tables.
fn media_information(
    layout: &Layout<'_>,
    entries: &[Entry],
    deltas: &[u32],
    chunk_offset: u64,
    wide: bool,
) -> Result<Vec<u8>, ContainerError> {
    let mut information = Vec::new();
    let mut video_header = Vec::with_capacity(8);
    push_u16(&mut video_header, 0); // graphics mode: copy
    push_u16(&mut video_header, 0);
    push_u16(&mut video_header, 0);
    push_u16(&mut video_header, 0);
    information.extend_from_slice(&full_box(*b"vmhd", 0, 1, &video_header));
    information.extend_from_slice(&data_information());
    information.extend_from_slice(&sample_table(layout, entries, deltas, chunk_offset, wide)?);
    Ok(boxed(*b"minf", &information))
}

/// Renders the self-contained data reference.
fn data_information() -> Vec<u8> {
    let mut entry = Vec::new();
    let mut references = Vec::new();
    push_u32(&mut references, 1);
    references.extend_from_slice(&full_box(*b"url ", 0, 1, &[]));
    entry.extend_from_slice(&full_box(*b"dref", 0, 0, &references));
    boxed(*b"dinf", &entry)
}

/// Renders `stbl` and its sample tables.
fn sample_table(
    layout: &Layout<'_>,
    entries: &[Entry],
    deltas: &[u32],
    chunk_offset: u64,
    wide: bool,
) -> Result<Vec<u8>, ContainerError> {
    let mut table = Vec::new();
    table.extend_from_slice(&sample_description(layout)?);
    table.extend_from_slice(&time_to_sample(deltas));
    table.extend_from_slice(&sample_to_chunk(entries.len())?);
    table.extend_from_slice(&sample_sizes(entries)?);
    table.extend_from_slice(&chunk_offsets(entries.len(), chunk_offset, wide)?);
    table.extend_from_slice(&sync_samples(entries));
    Ok(boxed(*b"stbl", &table))
}

/// Renders the single `avc1` sample entry with its `avcC` configuration.
fn sample_description(layout: &Layout<'_>) -> Result<Vec<u8>, ContainerError> {
    let width = u16::try_from(layout.width).map_err(|_| ContainerError::Unsupported)?;
    let height = u16::try_from(layout.height).map_err(|_| ContainerError::Unsupported)?;
    let mut entry = Vec::with_capacity(80);
    entry.extend_from_slice(&[0; 6]);
    push_u16(&mut entry, 1); // data reference index
    push_u16(&mut entry, 0); // pre-defined
    push_u16(&mut entry, 0); // reserved
    entry.extend_from_slice(&[0; 12]); // pre-defined
    push_u16(&mut entry, width);
    push_u16(&mut entry, height);
    push_u32(&mut entry, 0x0048_0000); // 72 dpi horizontal
    push_u32(&mut entry, 0x0048_0000); // 72 dpi vertical
    push_u32(&mut entry, 0); // reserved
    push_u16(&mut entry, 1); // frame count
    let mut compressor = [0_u8; 32];
    compressor[0] = u8::try_from(COMPRESSOR.len()).map_err(|_| ContainerError::Unsupported)?;
    compressor[1..=COMPRESSOR.len()].copy_from_slice(COMPRESSOR.as_bytes());
    entry.extend_from_slice(&compressor);
    push_u16(&mut entry, 0x0018); // depth
    push_u16(&mut entry, 0xFFFF); // pre-defined
    entry.extend_from_slice(&boxed(
        *b"avcC",
        &layout.config.with_centered_chroma()?.record()?,
    ));
    // MP4 has no separate chroma-siting field; nclx carries the matrix, range,
    // and primaries so players do not infer a different matrix from the stream.
    let colour = Colour::BT709_LIMITED_CENTERED;
    let mut colour_box = Vec::with_capacity(11);
    colour_box.extend_from_slice(b"nclx");
    push_u16(&mut colour_box, colour.primaries);
    push_u16(&mut colour_box, colour.transfer);
    push_u16(&mut colour_box, colour.matrix);
    colour_box.push(0); // limited range: the flag bit is clear
    entry.extend_from_slice(&boxed(*b"colr", &colour_box));

    let mut description = Vec::new();
    push_u32(&mut description, 1); // entry count
    description.extend_from_slice(&boxed(*b"avc1", &entry));
    Ok(full_box(*b"stsd", 0, 0, &description))
}

/// Renders the decoding time-to-sample table, run-length compressed.
fn time_to_sample(deltas: &[u32]) -> Vec<u8> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for delta in deltas {
        match runs.last_mut() {
            Some((count, previous)) if previous == delta => {
                *count = count.saturating_add(1);
            }
            _ => runs.push((1, *delta)),
        }
    }
    let mut payload = Vec::with_capacity(4 + runs.len() * 8);
    push_u32(&mut payload, u32::try_from(runs.len()).unwrap_or(u32::MAX));
    for (count, delta) in runs {
        push_u32(&mut payload, count);
        push_u32(&mut payload, delta);
    }
    full_box(*b"stts", 0, 0, &payload)
}

/// Renders the sample-to-chunk table: one chunk holds every sample.
fn sample_to_chunk(count: usize) -> Result<Vec<u8>, ContainerError> {
    let entries = u32::from(count != 0);
    let mut payload = Vec::with_capacity(4 + 12);
    push_u32(&mut payload, entries);
    if count != 0 {
        push_u32(&mut payload, 1); // first chunk
        push_u32(
            &mut payload,
            u32::try_from(count).map_err(|_| ContainerError::Unsupported)?,
        );
        push_u32(&mut payload, 1); // sample description index
    }
    Ok(full_box(*b"stsc", 0, 0, &payload))
}

/// Renders the per-sample size table.
fn sample_sizes(entries: &[Entry]) -> Result<Vec<u8>, ContainerError> {
    let mut payload = Vec::with_capacity(8 + entries.len() * 4);
    push_u32(&mut payload, 0); // constant sample size follows per-sample
    push_u32(
        &mut payload,
        u32::try_from(entries.len()).map_err(|_| ContainerError::Unsupported)?,
    );
    for entry in entries {
        push_u32(&mut payload, entry.size);
    }
    Ok(full_box(*b"stsz", 0, 0, &payload))
}

/// Renders the chunk offset table, widening to 64-bit offsets when needed.
fn chunk_offsets(count: usize, offset: u64, wide: bool) -> Result<Vec<u8>, ContainerError> {
    let entries = u32::from(count != 0);
    let mut payload = Vec::with_capacity(4 + if wide { 12 } else { 8 });
    push_u32(&mut payload, entries);
    if count != 0 {
        if wide {
            push_u64(&mut payload, offset);
        } else {
            push_u32(
                &mut payload,
                u32::try_from(offset).map_err(|_| ContainerError::Unsupported)?,
            );
        }
    }
    Ok(if wide {
        full_box(*b"co64", 0, 0, &payload)
    } else {
        full_box(*b"stco", 0, 0, &payload)
    })
}

/// Renders the sync sample table with one-based sample numbers.
fn sync_samples(entries: &[Entry]) -> Vec<u8> {
    let sync = entries.iter().filter(|entry| entry.keyframe).count();
    let mut payload = Vec::with_capacity(4 + sync * 4);
    push_u32(&mut payload, u32::try_from(sync).unwrap_or(u32::MAX));
    for (index, entry) in entries.iter().enumerate() {
        if entry.keyframe {
            push_u32(&mut payload, u32::try_from(index + 1).unwrap_or(u32::MAX));
        }
    }
    full_box(*b"stss", 0, 0, &payload)
}

/// Returns the size of the `mdat` header for `data_length` payload bytes.
fn mdat_header_length(data_length: u64) -> usize {
    if u32::try_from(data_length.saturating_add(8)).is_ok() {
        8
    } else {
        16
    }
}

/// Appends the `mdat` header, using a 64-bit size when the box is too large.
fn write_mdat_header(out: &mut Vec<u8>, data_length: u64) {
    if let Ok(size) = u32::try_from(data_length.saturating_add(8)) {
        push_u32(out, size);
        out.extend_from_slice(b"mdat");
    } else {
        push_u32(out, 1); // large size follows
        out.extend_from_slice(b"mdat");
        push_u64(out, data_length.saturating_add(16));
    }
}

/// Wraps a payload in a box.
fn boxed(id: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    let size = u32::try_from(payload.len() + 8).unwrap_or(u32::MAX);
    push_u32(&mut out, size);
    out.extend_from_slice(&id);
    out.extend_from_slice(payload);
    out
}

/// Wraps a payload in a full box with a version and flags.
fn full_box(id: [u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(payload.len() + 4);
    body.push(version);
    body.extend_from_slice(&[
        ((flags >> 16) & 0xFF) as u8,
        ((flags >> 8) & 0xFF) as u8,
        (flags & 0xFF) as u8,
    ]);
    body.extend_from_slice(payload);
    boxed(id, &body)
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests;
