//! Reconstruction of sealed segments after an interrupted session.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use cvbbmso_protocol::SegmentMeta;

use super::{META_EXTENSION, ORDINAL_WIDTH, VIDEO_EXTENSION, file_name};
use crate::RecordingError;

/// Restores the segments found in `segments_dir`.
///
/// A segment counts only when its video and metadata files both exist, the
/// metadata parses, and its index matches the file name. Files left behind by an
/// interrupted seal are ignored so their index can be reused.
///
/// The video digest is not recomputed: metadata is published only after the
/// video was written and synchronized, so its presence already proves the
/// segment is complete.
///
/// Only the longest run that starts at index one is restored. A manifest must be
/// non-empty and contiguous, so a segment after a gap could never be reported,
/// and keeping it would stop its index from being recorded again.
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
            break;
        }
        let Ok(bytes) = fs::read(segments_dir.join(file_name(*index, META_EXTENSION))) else {
            break;
        };
        let Ok(meta) = serde_json::from_slice::<SegmentMeta>(&bytes) else {
            break;
        };
        if meta.index != *index {
            break;
        }
        segments.insert(*index, meta);
        expected = expected.saturating_add(1);
    }
    Ok(segments)
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
