use cvbbmso_protocol::{
    ClientIncident, ClientState, ExamEvent, ExecutionState, IncidentCode, RecordingManifest,
    RecordingState, SegmentMeta, Sha256Digest,
};
use time::OffsetDateTime;

use super::{CORRUPT_FILE, JOURNAL_FILE, Journal, RUNTIME_DIR};

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_797_729_600 + seconds).unwrap()
}

fn segment(index: u32) -> ExamEvent {
    let start = i64::from(index) * 30;
    ExamEvent::SegmentFinalized(
        SegmentMeta::new(
            index,
            at(start),
            at(start + 30),
            Sha256Digest::parse(&"a".repeat(64)).unwrap(),
        )
        .unwrap(),
    )
}

fn incident(seconds: i64) -> ExamEvent {
    ExamEvent::Incident(ClientIncident {
        code: IncidentCode::NavigationDenied,
        occurred_at: at(seconds),
        detail: None,
    })
}

fn capturing() -> ClientState {
    ClientState {
        execution: ExecutionState::Recording,
        recording: RecordingState::Capturing,
    }
}

fn manifest() -> ExamEvent {
    let ExamEvent::SegmentFinalized(meta) = segment(1) else {
        unreachable!()
    };
    ExamEvent::RecordingFinalized(
        RecordingManifest::new(vec![meta], Sha256Digest::parse(&"b".repeat(64)).unwrap()).unwrap(),
    )
}

fn seqs(journal: &Journal) -> Vec<i64> {
    journal.messages_after(0).map(|(seq, _)| seq).collect()
}

async fn open(dir: &tempfile::TempDir) -> Journal {
    Journal::open(dir.path()).await.unwrap()
}

fn write_legacy_snapshot(dir: &tempfile::TempDir, seq: u64) {
    let runtime = dir.path().join(RUNTIME_DIR);
    std::fs::create_dir_all(&runtime).unwrap();
    let next_seq = u128::from(seq) + 1;
    let event = serde_json::to_string(&incident(0)).unwrap();
    std::fs::write(
        runtime.join(JOURNAL_FILE),
        format!(
            r#"{{"next_seq":{next_seq},"queue":[{{"seq":{seq},"event":{event}}}],"segments":[],"finalized":false,"state":{{"execution":"ready","recording":"idle"}}}}"#
        ),
    )
    .unwrap();
}

#[tokio::test]
async fn events_wait_for_the_first_sync_to_be_numbered() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;

    assert!(journal.append(segment(1)).is_none());
    assert!(journal.append(incident(40)).is_none());

    journal.anchor(0);
    assert_eq!(seqs(&journal), [1, 2]);
    assert_eq!(journal.append(segment(2)), Some(3));
}

#[tokio::test]
async fn a_fresh_journal_continues_after_the_server_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.append(segment(1));

    // The server already holds seq 1..=7 from a journal that was lost.
    journal.anchor(7);
    assert_eq!(seqs(&journal), [8]);
    assert_eq!(journal.append(incident(50)), Some(9));
}

#[tokio::test]
async fn sync_skips_confirmed_events_on_replay_and_keeps_them_until_end_ack() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.anchor(0);
    for index in 1..=4 {
        journal.append(segment(index));
    }

    journal.anchor(2);
    assert_eq!(
        journal
            .messages_after(2)
            .map(|(seq, _)| seq)
            .collect::<Vec<_>>(),
        [3, 4]
    );
    journal.anchor(4);
    assert_eq!(journal.messages_after(4).count(), 0);
    assert_eq!(seqs(&journal), [1, 2, 3, 4]);
    journal.clear_events();
    assert!(seqs(&journal).is_empty());
}

#[tokio::test]
async fn a_server_watermark_ahead_of_a_legacy_snapshot_preserves_retained_events() {
    let dir = tempfile::tempdir().unwrap();
    write_legacy_snapshot(&dir, 3);
    let mut journal = open(&dir).await;
    assert_eq!(seqs(&journal), [3]);

    journal.anchor(7);
    assert_eq!(journal.append(segment(1)), Some(8));
    journal.save().await.unwrap();

    let mut journal = open(&dir).await;
    assert_eq!(seqs(&journal), [3, 8]);
    journal.anchor(7);
    let (seq, _) = journal.messages_after(7).next().unwrap();
    assert_eq!(seq, 8);
    assert_eq!(journal.append(incident(10)), Some(9));
}

#[tokio::test]
async fn numbering_never_moves_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.anchor(0);
    journal.append(segment(1));
    journal.append(segment(2));

    // A stale or lower acknowledgement must not reissue seq 1 or 2.
    journal.anchor(0);
    assert_eq!(seqs(&journal), [1, 2]);
    assert_eq!(journal.append(segment(3)), Some(3));
}

#[tokio::test]
async fn recovered_segments_are_not_queued_twice() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.anchor(0);

    assert_eq!(journal.append(segment(1)), Some(1));
    // Still pending: the original entry keeps its sequence number.
    assert!(journal.append(segment(1)).is_none());
    journal.anchor(1);
    // Already confirmed: nothing is sent again.
    assert!(journal.append(segment(1)).is_none());
    assert_eq!(journal.append(segment(2)), Some(2));
}

#[tokio::test]
async fn saved_state_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut journal = open(&dir).await;
        journal.anchor(3);
        journal.append(segment(1));
        journal.append(incident(45));
        journal.set_state(capturing());
        journal.save().await.unwrap();
    }

    let mut journal = open(&dir).await;
    assert_eq!(journal.state(), capturing());
    assert!(journal.append(segment(1)).is_none());
    journal.anchor(3);
    assert_eq!(seqs(&journal), [4, 5]);
    assert_eq!(journal.append(segment(2)), Some(6));
}

#[tokio::test]
async fn clearing_events_keeps_numbering_and_segment_identity() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.anchor(0);
    journal.append(segment(1));
    journal.append(incident(45));

    journal.clear_events();

    journal.anchor(0);
    assert!(seqs(&journal).is_empty());
    assert!(journal.append(segment(1)).is_none());
    assert_eq!(journal.append(incident(60)), Some(3));
}

#[tokio::test]
async fn a_corrupt_snapshot_is_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = dir.path().join(RUNTIME_DIR);
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(runtime.join(JOURNAL_FILE), b"{ not json").unwrap();

    let mut journal = open(&dir).await;

    assert_eq!(journal.state().execution, ExecutionState::Ready);
    assert!(runtime.join(CORRUPT_FILE).exists());
    assert!(journal.append(segment(1)).is_none());
}

#[tokio::test]
async fn a_snapshot_with_noncontiguous_sequences_is_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.anchor(0);
    journal.append(segment(1));
    journal.data.next_seq = Some(3);
    journal.save().await.unwrap();

    let mut journal = open(&dir).await;
    assert!(dir.path().join(RUNTIME_DIR).join(CORRUPT_FILE).exists());
    journal.anchor(1);
    assert_eq!(journal.append(segment(1)), Some(2));
}

#[tokio::test]
async fn a_completed_snapshot_requires_a_recorded_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.set_state(ClientState {
        execution: ExecutionState::Finalizing,
        recording: RecordingState::Complete,
    });
    journal.save().await.unwrap();

    let journal = open(&dir).await;
    assert!(dir.path().join(RUNTIME_DIR).join(CORRUPT_FILE).exists());
    assert_eq!(journal.state().execution, ExecutionState::Ready);
}

#[tokio::test]
async fn sequence_exhaustion_survives_reopening_without_wrapping() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.append(segment(1));
    journal.append(segment(2));
    journal.anchor(i64::MAX - 1);
    assert_eq!(seqs(&journal), [i64::MAX]);
    assert!(journal.has_unsendable_events());
    journal.save().await.unwrap();

    let mut journal = open(&dir).await;
    assert_eq!(seqs(&journal), [i64::MAX]);
    journal.anchor(i64::MAX);
    assert_eq!(journal.append(segment(3)), None);
    assert!(journal.has_unsendable_events());
    assert_eq!(seqs(&journal), [i64::MAX]);
    assert_eq!(journal.messages_after(i64::MAX).count(), 0);
}

#[tokio::test]
async fn a_legacy_unsigned_sequence_beyond_the_wire_limit_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    write_legacy_snapshot(&dir, u64::MAX);
    let mut journal = open(&dir).await;
    assert!(journal.has_unsendable_events());
    assert_eq!(journal.messages_after(0).count(), 0);
    journal.anchor(i64::MAX);
    assert_eq!(journal.append(segment(1)), None);
    journal.save().await.unwrap();

    let journal = open(&dir).await;
    assert!(journal.has_unsendable_events());
    assert_eq!(journal.data.queue.len(), 2);
    assert_eq!(journal.data.queue[0].seq, Some(u64::MAX));
    assert_eq!(journal.data.queue[0].event, incident(0));
    assert_eq!(journal.data.queue[1].event, segment(1));
}

#[tokio::test]
async fn saving_does_not_follow_a_preexisting_temporary_path() {
    let dir = tempfile::tempdir().unwrap();
    let journal = open(&dir).await;
    let unrelated = dir.path().join("unrelated");
    std::fs::write(&unrelated, "original").unwrap();
    let stale = dir.path().join(RUNTIME_DIR).join("journal.json.tmp");
    std::fs::hard_link(&unrelated, &stale).unwrap();

    journal.save().await.unwrap();

    assert_eq!(std::fs::read_to_string(&unrelated).unwrap(), "original");
    assert_eq!(std::fs::read_to_string(&stale).unwrap(), "original");
    assert!(dir.path().join(RUNTIME_DIR).join(JOURNAL_FILE).is_file());
}

#[tokio::test]
async fn the_recording_manifest_is_queued_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.anchor(0);

    assert_eq!(journal.append(manifest()), Some(1));
    assert!(journal.append(manifest()).is_none());
}
