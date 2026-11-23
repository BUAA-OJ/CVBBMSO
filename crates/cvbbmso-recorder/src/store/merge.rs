//! Merging sealed segments into the final video.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::Path,
};

use cvbbmso_protocol::Sha256Digest;
use sha2::{Digest, Sha256};

use super::SegmentStore;
use crate::RecordingError;
use crate::container::{MkvFile, Mp4Writer};

impl SegmentStore {
    /// The first sealed track defines the format a resumed capture must use.
    pub(crate) fn existing_video_layout(
        &self,
    ) -> Result<Option<(u32, u32, crate::container::AvcConfig)>, RecordingError> {
        self.segments
            .first_key_value()
            .map(|(_, meta)| {
                let file = self.parse_segment(meta)?;
                Ok((file.width, file.height, file.config))
            })
            .transpose()
    }

    /// Merges every sealed segment into `final.mp4` and returns its digest.
    ///
    /// Segments are concatenated in media order: the first sample of a segment
    /// follows the last sample of the previous one, and each segment's own first
    /// timestamp is treated as zero. The wall-clock span remains in the segment
    /// metadata, which is the manifest's evidence. The final segment's tail
    /// duration is passed explicitly to the MP4 writer.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::NoSegments`] when nothing was sealed, and
    /// [`RecordingError::Merge`] when a segment is missing, no longer matches its
    /// digest, exceeds the configured read limit, cannot be parsed, disagrees
    /// with the other segments about the video layout, or the final file cannot
    /// be written.
    pub(crate) fn merge(&self) -> Result<Sha256Digest, RecordingError> {
        let segments = self.segments();
        if segments.is_empty() {
            return Err(RecordingError::NoSegments);
        }

        let mut writer: Option<Mp4Writer> = None;
        let mut layout: Option<(crate::container::AvcConfig, u32, u32)> = None;
        let mut cursor = 0_u64;
        let mut final_tail = 1_u64;
        for meta in &segments {
            let file = self.parse_segment(meta)?;
            let current = (file.config.clone(), file.width, file.height);
            match &layout {
                Some(expected) if expected != &current => {
                    return Err(merge_error(format!(
                        "segment {} disagrees with the recording video layout",
                        meta.index
                    )));
                }
                Some(_) => {}
                None => {
                    writer = Some(Mp4Writer::new(file.config.clone(), file.width, file.height));
                    layout = Some(current);
                }
            }
            let Some(writer) = writer.as_mut() else {
                return Err(merge_error("segment writer was not initialized"));
            };
            let origin = file.samples.first().map_or(0, |sample| sample.timestamp_ms);
            let mut last = None;
            for sample in &file.samples {
                // Each segment owns its own zero point. Normalising it here keeps
                // a late first sample from becoming a frozen gap after rotation.
                let relative = sample.timestamp_ms.checked_sub(origin).ok_or_else(|| {
                    merge_error(format!("segment {} samples are out of order", meta.index))
                })?;
                let timestamp = cursor.checked_add(relative).ok_or_else(|| {
                    merge_error("merged timeline exceeds the representable range")
                })?;
                writer
                    .push(timestamp, sample.keyframe, sample.data.len())
                    .map_err(merge_container)?;
                last = Some(timestamp);
            }
            if let Some(last) = last {
                final_tail = tail_duration(&file.samples);
                cursor = last.checked_add(final_tail).ok_or_else(|| {
                    merge_error("merged timeline exceeds the representable range")
                })?;
            }
        }
        let Some(writer) = writer.as_mut() else {
            return Err(merge_error("segment writer was not initialized"));
        };
        writer.set_tail_duration(
            u32::try_from(final_tail)
                .map_err(|_| merge_error("final frame duration does not fit the MP4 table"))?,
        );

        let header = writer.start().map_err(merge_container)?;
        let expected = writer.data_length();
        let temporary = self.final_temporary_path();
        // Create this attempt's file exclusively. An existing file may belong to
        // a merge that is still writing, so it is never truncated or replaced.
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    merge_error(format!(
                        "temporary file {} already exists; refusing to overwrite it",
                        temporary.display()
                    ))
                } else {
                    merge_failure(error)
                }
            })?;
        if let Err(error) = self.write_final(output, &header, expected, &segments) {
            remove_temporary(&temporary);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temporary, self.final_path()) {
            remove_temporary(&temporary);
            return Err(merge_failure(error));
        }
        super::hash_file(&self.final_path()).map_err(merge_failure)
    }

    /// Writes the MP4 header and every segment payload to the exclusive output.
    fn write_final(
        &self,
        output: File,
        header: &[u8],
        expected: u64,
        segments: &[cvbbmso_protocol::SegmentMeta],
    ) -> Result<(), RecordingError> {
        let mut output = BufWriter::new(output);
        output.write_all(header).map_err(merge_failure)?;
        let mut written = 0_u64;
        for meta in segments {
            let file = self.parse_segment(meta)?;
            for sample in &file.samples {
                output.write_all(&sample.data).map_err(merge_failure)?;
                written =
                    written
                        .checked_add(u64::try_from(sample.data.len()).map_err(|_| {
                            merge_error("sample length does not fit the merged file")
                        })?)
                        .ok_or_else(|| merge_error("merged file length overflows"))?;
            }
        }
        if written != expected {
            return Err(merge_error(format!(
                "merged payload is {written} bytes, expected {expected}"
            )));
        }
        let file = output
            .into_inner()
            .map_err(|error| merge_failure(error.into_error()))?;
        file.sync_all().map_err(merge_failure)
    }

    /// Reads and verifies one sealed segment.
    fn parse_segment(
        &self,
        meta: &cvbbmso_protocol::SegmentMeta,
    ) -> Result<MkvFile, RecordingError> {
        let bytes = super::read_bounded(&self.video_path(meta.index), self.limits.segment_video)
            .map_err(merge_failure)?;
        let digest = digest_of(&bytes)?;
        if digest != meta.sha256 {
            return Err(merge_error(format!(
                "segment {} no longer matches its sealed digest",
                meta.index
            )));
        }
        let mut file = MkvFile::parse(&bytes).map_err(merge_container)?;
        // Recovery may encounter older segments whose in-band SPS omitted VUI.
        // Rewrite both passes so MP4 sample sizes match the bytes actually copied.
        // The sealed segment and the digest checked above remain untouched.
        for sample in &mut file.samples {
            sample.data =
                crate::container::with_centered_chroma(&sample.data).map_err(merge_container)?;
        }
        Ok(file)
    }
}

/// Removes the temporary file created by this merge attempt.
///
/// Cleanup is best effort: returning the original merge failure is more useful
/// than replacing it with a cleanup failure, and a leftover file is never
/// published or listed in the manifest.
fn remove_temporary(path: &Path) {
    let _ = fs::remove_file(path);
}

/// Finishes the timeline after the last sample of a segment.
fn tail_duration(samples: &[crate::container::Sample]) -> u64 {
    let Some(last) = samples.last() else {
        return 1;
    };
    samples
        .iter()
        .rev()
        .nth(1)
        .map_or(1, |previous| {
            last.timestamp_ms.saturating_sub(previous.timestamp_ms)
        })
        .max(1)
}

/// Computes the digest of one segment's bytes.
fn digest_of(bytes: &[u8]) -> Result<Sha256Digest, RecordingError> {
    let digest = Sha256::digest(bytes);
    Sha256Digest::parse(&super::hexadecimal(&digest))
        .map_err(|error| merge_error(format!("digest is not representable: {error}")))
}

/// Wraps an I/O failure from the merge step.
fn merge_failure(error: io::Error) -> RecordingError {
    RecordingError::Merge(Some(error))
}

/// Preserves a container failure as the source of a merge error.
fn merge_container(error: impl std::error::Error + Send + Sync + 'static) -> RecordingError {
    RecordingError::Merge(Some(io::Error::new(io::ErrorKind::InvalidData, error)))
}

/// Builds a merge failure with a descriptive cause.
fn merge_error(message: impl Into<String>) -> RecordingError {
    RecordingError::Merge(Some(io::Error::new(
        io::ErrorKind::InvalidData,
        message.into(),
    )))
}

#[cfg(test)]
mod tests;
