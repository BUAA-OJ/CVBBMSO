use std::{fs, path::Path, path::PathBuf};

use cvbbmso_protocol::{RecordingManifest, RecordingPolicy, SegmentMeta, Sha256Digest};
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::sync::mpsc::Receiver;
use uuid::Uuid;

use super::{Recorder, State};
use crate::{RecorderEvent, RecordingError, store::SegmentStore};

const OJ_USER_ID: i64 = 42;

fn exam_id() -> Uuid {
    Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0010)
}

fn policy() -> RecordingPolicy {
    RecordingPolicy::new(30, 15).expect("policy")
}

fn candidate_dir(directory: &TempDir, user: i64) -> PathBuf {
    directory
        .path()
        .join(exam_id().to_string())
        .join(user.to_string())
}

fn timestamp(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).expect("timestamp in range")
}

fn digest() -> Sha256Digest {
    Sha256Digest::parse(&"a".repeat(64)).expect("digest")
}

fn manifest() -> RecordingManifest {
    let segment = SegmentMeta::new(1, timestamp(0), timestamp(30), digest()).expect("segment");
    RecordingManifest::new(vec![segment], digest()).expect("manifest")
}

fn seal_segments(candidate: &Path, count: u32) {
    let mut store = SegmentStore::open(candidate).expect("store");
    for index in 1..=count {
        fs::write(store.video_path(index), b"abc").expect("video");
        store
            .seal(index, timestamp(0), timestamp(30))
            .expect("seal");
    }
}

async fn open_empty() -> (Recorder, Receiver<RecorderEvent>, TempDir) {
    let directory = TempDir::new().expect("temp dir");
    let candidate = candidate_dir(&directory, OJ_USER_ID);
    let (recorder, events) = Recorder::open(exam_id(), OJ_USER_ID, candidate, policy())
        .await
        .expect("open");
    (recorder, events, directory)
}

async fn open_with_segments(count: u32) -> (Recorder, Receiver<RecorderEvent>, TempDir) {
    let directory = TempDir::new().expect("temp dir");
    let candidate = candidate_dir(&directory, OJ_USER_ID);
    seal_segments(&candidate, count);
    let (recorder, events) = Recorder::open(exam_id(), OJ_USER_ID, candidate, policy())
        .await
        .expect("open");
    (recorder, events, directory)
}

#[tokio::test]
async fn open_rejects_a_directory_of_another_candidate() {
    let directory = TempDir::new().expect("temp dir");
    let candidate = candidate_dir(&directory, OJ_USER_ID + 1);

    let result = Recorder::open(exam_id(), OJ_USER_ID, candidate, policy()).await;

    assert!(matches!(result, Err(RecordingError::Io(_))));
}

#[tokio::test]
async fn open_reports_restored_segments_in_order() {
    let directory = TempDir::new().expect("temp dir");
    let candidate = candidate_dir(&directory, OJ_USER_ID);
    seal_segments(&candidate, 2);

    let (_recorder, mut events) = Recorder::open(exam_id(), OJ_USER_ID, candidate, policy())
        .await
        .expect("open");

    for expected in 1..=2_u32 {
        match events.recv().await.expect("event") {
            RecorderEvent::SegmentFinalized(meta) => assert_eq!(meta.index, expected),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}

#[tokio::test]
async fn start_reports_the_platform_error() {
    let (mut recorder, _events, _directory) = open_empty().await;

    assert!(matches!(
        recorder.start().await,
        Err(RecordingError::Platform(_))
    ));
}

#[tokio::test]
async fn start_is_idempotent_while_capturing() {
    let (mut recorder, _events, _directory) = open_empty().await;
    recorder.state = State::Capturing;

    assert!(recorder.start().await.is_ok());
}

#[tokio::test]
async fn stop_returns_a_failed_recorder_to_the_opened_state() {
    let (mut recorder, _events, _directory) = open_empty().await;
    recorder.state = State::Failed;

    recorder.stop().await.expect("stop");

    assert!(matches!(recorder.state, State::Opened));
}

#[tokio::test]
async fn stop_is_idempotent() {
    let (mut recorder, _events, _directory) = open_empty().await;

    recorder.stop().await.expect("first stop");
    recorder.stop().await.expect("second stop");

    assert!(matches!(recorder.state, State::Opened));
}

#[tokio::test]
async fn finish_without_segments_reports_no_segments() {
    let (mut recorder, _events, _directory) = open_empty().await;

    assert!(matches!(
        recorder.finish().await,
        Err(RecordingError::NoSegments)
    ));
}

#[tokio::test]
async fn finish_returns_the_same_manifest_after_completion() {
    let (mut recorder, _events, _directory) = open_empty().await;
    let cached = manifest();
    recorder.state = State::Complete(cached.clone());

    assert_eq!(recorder.finish().await.expect("manifest"), cached);
}

#[tokio::test]
async fn finish_reports_merge_and_stays_retryable() {
    let (mut recorder, _events, _directory) = open_with_segments(1).await;

    assert!(matches!(
        recorder.finish().await,
        Err(RecordingError::Merge)
    ));
    assert!(matches!(recorder.state, State::Failed));
    assert!(matches!(
        recorder.finish().await,
        Err(RecordingError::Merge)
    ));
}

#[tokio::test]
async fn open_fails_when_sealed_metadata_cannot_be_read() {
    let directory = TempDir::new().expect("temp dir");
    let candidate = candidate_dir(&directory, OJ_USER_ID);
    seal_segments(&candidate, 1);

    let segments = candidate.join("recording").join("segments");
    let metadata = segments.join("000001.json");
    let backup = segments.join("000001.json.bak");
    fs::rename(&metadata, &backup).expect("move metadata aside");
    fs::create_dir(&metadata).expect("block the metadata path");

    let result = Recorder::open(exam_id(), OJ_USER_ID, candidate.clone(), policy()).await;

    assert!(matches!(result, Err(RecordingError::Io(_))));
    assert!(
        segments.join("000001.mkv").is_file(),
        "the sealed video must survive a failed recovery"
    );

    fs::remove_dir(&metadata).expect("unblock the metadata path");
    fs::rename(&backup, &metadata).expect("restore metadata");

    let (_recorder, mut events) = Recorder::open(exam_id(), OJ_USER_ID, candidate, policy())
        .await
        .expect("open once the fault clears");

    match events.recv().await.expect("event") {
        RecorderEvent::SegmentFinalized(meta) => assert_eq!(meta.index, 1),
        other => panic!("unexpected event: {other:?}"),
    }
}
