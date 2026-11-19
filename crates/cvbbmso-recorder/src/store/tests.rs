use std::fs;

use cvbbmso_protocol::{SegmentMeta, Sha256Digest};
use tempfile::TempDir;
use time::OffsetDateTime;

use super::SegmentStore;
use crate::RecordingError;

/// SHA-256 of an empty file.
const EMPTY_DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// SHA-256 of `b"abc"`.
const ABC_DIGEST: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn timestamp(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).expect("timestamp in range")
}

fn store(directory: &TempDir) -> SegmentStore {
    SegmentStore::open(directory.path()).expect("store")
}

fn write_video(store: &SegmentStore, index: u32, content: &[u8]) {
    fs::write(store.video_path(index), content).expect("video");
}

fn segments_dir(directory: &TempDir) -> std::path::PathBuf {
    directory.path().join("recording").join("segments")
}

#[test]
fn open_creates_the_recording_directories() {
    let directory = TempDir::new().expect("temp dir");
    let store = store(&directory);

    assert!(segments_dir(&directory).is_dir());
    assert_eq!(store.segments(), Vec::new());
    assert_eq!(store.next_index(), 1);
}

#[test]
fn seal_records_the_digest_of_the_video() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    write_video(&store, 1, b"abc");

    let meta = store.seal(1, timestamp(0), timestamp(30)).expect("seal");

    assert_eq!(meta.index, 1);
    assert_eq!(meta.sha256.as_str(), ABC_DIGEST);
    assert_eq!(store.next_index(), 2);

    let bytes = fs::read(segments_dir(&directory).join("000001.json")).expect("metadata");
    let restored: SegmentMeta = serde_json::from_slice(&bytes).expect("metadata json");
    assert_eq!(restored, meta);
}

#[test]
fn seal_hashes_an_empty_video() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    write_video(&store, 1, b"");

    let meta = store.seal(1, timestamp(0), timestamp(0)).expect("seal");

    assert_eq!(meta.sha256.as_str(), EMPTY_DIGEST);
}

#[test]
fn seal_rejects_an_index_that_does_not_follow() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    write_video(&store, 2, b"abc");

    assert!(matches!(
        store.seal(2, timestamp(0), timestamp(30)),
        Err(RecordingError::Io(_))
    ));
}

#[test]
fn reopening_restores_sealed_segments() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    for index in 1..=2_u32 {
        write_video(&store, index, b"abc");
        store
            .seal(index, timestamp(0), timestamp(30))
            .expect("seal");
    }
    drop(store);

    let reopened = SegmentStore::open(directory.path()).expect("reopen");

    let segments = reopened.segments();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].index, 1);
    assert_eq!(segments[1].index, 2);
    assert_eq!(segments[0].sha256.as_str(), ABC_DIGEST);
    assert_eq!(reopened.next_index(), 3);
}

#[test]
fn ignores_a_video_without_metadata() {
    let directory = TempDir::new().expect("temp dir");
    let store = store(&directory);
    write_video(&store, 5, b"abc");
    drop(store);

    let reopened = SegmentStore::open(directory.path()).expect("reopen");

    assert_eq!(reopened.segments(), Vec::new());
    assert_eq!(reopened.next_index(), 1);
}

#[test]
fn fails_on_unreadable_metadata() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    write_video(&store, 1, b"abc");
    store.seal(1, timestamp(0), timestamp(30)).expect("seal");
    write_video(&store, 2, b"abc");
    fs::write(segments_dir(&directory).join("000002.json"), b"{ not json")
        .expect("broken metadata");
    drop(store);

    assert!(SegmentStore::open(directory.path()).is_err());
}

#[test]
fn fails_when_a_sealed_segment_follows_a_gap() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    for index in 1..=3_u32 {
        write_video(&store, index, b"abc");
        store
            .seal(index, timestamp(0), timestamp(30))
            .expect("seal");
    }
    fs::remove_file(segments_dir(&directory).join("000002.json")).expect("remove metadata");
    drop(store);

    assert!(SegmentStore::open(directory.path()).is_err());
}

#[test]
fn fails_when_a_video_no_longer_matches_its_digest() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    write_video(&store, 1, b"abc");
    store.seal(1, timestamp(0), timestamp(30)).expect("seal");
    fs::write(store.video_path(1), b"tampered").expect("tamper");
    drop(store);

    assert!(SegmentStore::open(directory.path()).is_err());
}

#[test]
fn fails_when_a_sealed_video_is_missing() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    for index in 1..=2_u32 {
        write_video(&store, index, b"abc");
        store
            .seal(index, timestamp(0), timestamp(30))
            .expect("seal");
    }
    fs::remove_file(store.video_path(2)).expect("remove video");
    drop(store);

    assert!(SegmentStore::open(directory.path()).is_err());
}

#[test]
fn refuses_to_seal_over_existing_metadata() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    write_video(&store, 1, b"abc");
    store.seal(1, timestamp(0), timestamp(30)).expect("seal");
    write_video(&store, 2, b"abc");
    fs::write(segments_dir(&directory).join("000002.json"), b"{}").expect("existing metadata");

    assert!(matches!(
        store.seal(2, timestamp(0), timestamp(30)),
        Err(RecordingError::Io(_))
    ));
}

#[test]
fn manifest_lists_segments_in_ascending_order() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    for index in 1..=2_u32 {
        write_video(&store, index, b"abc");
        store
            .seal(index, timestamp(0), timestamp(30))
            .expect("seal");
    }

    let merged = Sha256Digest::parse(EMPTY_DIGEST).expect("digest");
    let manifest = store.manifest(merged).expect("manifest");

    assert_eq!(manifest.segments.len(), 2);
    assert_eq!(manifest.segments[0].index, 1);
    assert_eq!(manifest.segments[1].index, 2);
    assert_eq!(manifest.merged_sha256.as_str(), EMPTY_DIGEST);
}
