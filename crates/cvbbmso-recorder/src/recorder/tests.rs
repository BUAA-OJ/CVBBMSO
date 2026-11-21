use std::{fmt::Write as _, fs, path::Path, path::PathBuf};

use cvbbmso_protocol::{RecordingPolicy, RecordingSummary, Sha256Digest};
use sha2::Digest as _;
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::sync::mpsc::Receiver;
use uuid::Uuid;

use super::{Recorder, State};
use crate::container::{AvcConfig, MkvWriter, Sample};
use crate::{RecorderEvent, RecordingError, store::SegmentStore};

mod pipeline;

const SEQUENCE: [u8; 24] = [
    0x67, 0x4D, 0x40, 0x15, 0x95, 0xA0, 0x50, 0x7E, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0x78, 0x36,
    0x82, 0x21, 0x1A, 0x80,
];
const PICTURE: [u8; 4] = [0x68, 0xCE, 0x38, 0x80];

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

fn summary() -> RecordingSummary {
    RecordingSummary::new(1, digest()).expect("summary")
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

/// Writes `count` readable segments, each with two H.264 samples.
fn seal_video_segments(candidate: &Path, count: u32) {
    let mut store = SegmentStore::open(candidate).expect("store");
    let config = AvcConfig::new(SEQUENCE.to_vec(), PICTURE.to_vec()).expect("configuration");
    for index in 1..=count {
        let mut writer = MkvWriter::new(config.clone(), 320, 240);
        for (offset, keyframe) in [(0_u64, true), (66, false)] {
            writer
                .push(Sample {
                    timestamp_ms: offset,
                    keyframe,
                    data: vec![0, 0, 0, 4, 0x41, 0x80, 0x80, 0x80],
                })
                .expect("sample");
        }
        let bytes = writer.finish().expect("segment");
        fs::write(store.video_path(index), bytes).expect("video");
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

async fn open_with_video_segments(count: u32) -> (Recorder, Receiver<RecorderEvent>, TempDir) {
    let directory = TempDir::new().expect("temp dir");
    let candidate = candidate_dir(&directory, OJ_USER_ID);
    seal_video_segments(&candidate, count);
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
#[cfg(not(any(windows, target_os = "macos")))]
async fn start_reports_the_platform_error() {
    let (mut recorder, _events, _directory) = open_empty().await;

    assert!(matches!(
        recorder.start().await,
        Err(RecordingError::Platform(_))
    ));
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
async fn finish_returns_the_same_summary_after_completion() {
    let (mut recorder, _events, _directory) = open_empty().await;
    let cached = summary();
    recorder.state = State::Complete(cached.clone());

    assert_eq!(recorder.finish().await.expect("summary"), cached);
}

#[tokio::test]
async fn finish_merges_the_sealed_segments_and_caches_the_summary() {
    let (mut recorder, _events, directory) = open_with_video_segments(2).await;

    let summary = recorder.finish().await.expect("summary");
    let final_path = candidate_dir(&directory, OJ_USER_ID)
        .join("recording")
        .join("final.mp4");

    assert_eq!(summary.segment_count.get(), 2);
    assert!(final_path.is_file());
    assert_eq!(recorder.finish().await.expect("cached"), summary);
}

#[tokio::test]
async fn finish_can_merge_when_the_event_receiver_is_dropped() {
    let (mut recorder, events, _directory) = open_with_video_segments(2).await;
    drop(events);

    let summary = recorder.finish().await.expect("local summary");
    assert_eq!(summary.segment_count.get(), 2);
    assert!(recorder.store.final_path().is_file());
}

#[tokio::test]
async fn finish_reports_merge_and_stays_retryable() {
    let (mut recorder, _events, _directory) = open_with_segments(1).await;

    assert!(matches!(
        recorder.finish().await,
        Err(RecordingError::Merge(_))
    ));
    assert!(matches!(recorder.state, State::Failed));
    assert!(matches!(
        recorder.finish().await,
        Err(RecordingError::Merge(_))
    ));
}

/// Occupy the only blocking worker so cancelling both waits is deterministic.
#[test]
fn cancelled_finish_waits_reuse_the_merge_and_keep_its_result() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (mut recorder, _events, directory) = open_with_video_segments(1).await;
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let gate = tokio::task::spawn_blocking(move || {
            started.send(()).expect("signal worker");
            blocked.recv().expect("release worker");
        });
        ready.await.expect("worker started");

        for _ in 0..2 {
            let mut waiting = Box::pin(recorder.finish());
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(waiting.as_mut().poll(&mut context).is_pending());
            drop(waiting);
            recorder.stop().await.expect("stop retains merge");
        }
        release.send(()).expect("release");
        gate.await.expect("gate");
        wait_for_merge_completion(&recorder).await;

        // Starting a new merge would now fail. The original successful result
        // must still be returned, even when it completed without a waiter.
        fs::write(recorder.store.video_path(1), b"changed after merge").expect("change input");
        let result = recorder.finish().await.expect("original merge result");
        let final_path = candidate_dir(&directory, OJ_USER_ID).join("recording/final.mp4");
        let digest = sha2::Sha256::digest(fs::read(&final_path).expect("final file"))
            .iter()
            .fold(String::with_capacity(64), |mut text, byte| {
                write!(text, "{byte:02x}").expect("digest formatting");
                text
            });
        assert_eq!(result.merged_sha256.as_str(), digest);
        assert_eq!(recorder.finish().await.expect("cached result"), result);
    });
}

#[test]
fn cancelled_finish_preserves_a_failure_before_allowing_a_retry() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (mut recorder, _events, _directory) = open_with_video_segments(1).await;
        let video = recorder.store.video_path(1);
        let bytes = fs::read(&video).expect("sealed video");
        fs::remove_file(&video).expect("remove input");
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let gate = tokio::task::spawn_blocking(move || {
            started.send(()).expect("signal worker");
            blocked.recv().expect("release worker");
        });
        ready.await.expect("worker started");
        let mut waiting = Box::pin(recorder.finish());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(waiting.as_mut().poll(&mut context).is_pending());
        drop(waiting);
        release.send(()).expect("release");
        gate.await.expect("gate");
        wait_for_merge_completion(&recorder).await;
        fs::write(&video, bytes).expect("restore input");

        assert!(matches!(
            recorder.finish().await,
            Err(RecordingError::Merge(_))
        ));
        assert!(matches!(recorder.state, State::Failed));
        recorder
            .finish()
            .await
            .expect("retry after observing failure");
    });
}

async fn wait_for_merge_completion(recorder: &Recorder) {
    let State::Finalizing(task) = &recorder.state else {
        panic!("the recorder must retain the merge task");
    };
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
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
