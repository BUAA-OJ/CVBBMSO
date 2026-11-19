//! Reconstruction of sealed segments after an interrupted session.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use cvbbmso_protocol::SegmentMeta;

use super::{META_EXTENSION, ORDINAL_WIDTH, VIDEO_EXTENSION, file_name, hash_file, invalid_input};
use crate::RecordingError;

/// Restores the sealed segments of `segments_dir`.
///
/// A segment is sealed once its video and its metadata file both exist. Sealed
/// segments must form the run `1..=n`: a gap, an unreadable or malformed
/// metadata file, a mismatched index, or a video that no longer matches its
/// digest all fail recovery and leave every file untouched. Treating such a
/// segment as absent would let a later recording reuse an index that already
/// holds evidence.
///
/// A video without metadata was never sealed, so it is ignored and its index can
/// be recorded again. The digest is recomputed for every restored segment,
/// because the metadata only proves what the video looked like when it was
/// written, not that it is intact now.
pub(super) fn recover(segments_dir: &Path) -> Result<BTreeMap<u32, SegmentMeta>, RecordingError> {
    let mut videos = BTreeSet::new();
    let mut metas = BTreeSet::new();
    for entry in fs::read_dir(segments_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some((index, extension)) = classify(name) else {
            continue;
        };
        if extension == VIDEO_EXTENSION {
            videos.insert(index);
        } else {
            metas.insert(index);
        }
    }

    let mut segments = BTreeMap::new();
    let mut expected = 1_u32;
    for index in videos.intersection(&metas) {
        if *index != expected {
            return Err(invalid_input(format!(
                "sealed segment {index} follows a missing segment"
            )));
        }

        let meta_path = segments_dir.join(file_name(*index, META_EXTENSION));
        let meta = parse_meta(&meta_path, *index)?;

        let video_path = segments_dir.join(file_name(*index, VIDEO_EXTENSION));
        if hash_file(&video_path)? != meta.sha256 {
            return Err(invalid_input(format!(
                "segment {index} no longer matches its digest"
            )));
        }

        segments.insert(*index, meta);
        expected = expected.saturating_add(1);
    }
    Ok(segments)
}

/// Reads and validates the metadata file of `index`.
fn parse_meta(path: &Path, index: u32) -> Result<SegmentMeta, RecordingError> {
    let bytes = fs::read(path)?;
    let meta = serde_json::from_slice::<SegmentMeta>(&bytes)
        .map_err(|error| invalid_input(format!("segment {index} metadata: {error}")))?;
    if meta.index == index {
        Ok(meta)
    } else {
        Err(invalid_input(format!(
            "segment {index} metadata names index {}",
            meta.index
        )))
    }
}

/// Splits a segment file name into its index and extension.
fn classify(name: &str) -> Option<(u32, &str)> {
    let (stem, extension) = name.rsplit_once('.')?;
    if extension != VIDEO_EXTENSION && extension != META_EXTENSION {
        return None;
    }
    if stem.len() < ORDINAL_WIDTH || !stem.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((stem.parse().ok()?, extension))
}
