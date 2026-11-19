use cvbbmso_protocol::{
    ClientIncident, ClientMessage, ExamEvent, ExecutionState, IncidentCode, SegmentMeta,
    Sha256Digest,
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

fn seqs(messages: &[ClientMessage]) -> Vec<u64> {
    messages
        .iter()
        .map(|message| match message {
            ClientMessage::Event { seq, .. } => *seq,
            other => panic!("unexpected message {other:?}"),
        })
        .collect()
}

fn seq_of(message: Option<ClientMessage>) -> Option<u64> {
    message.map(|message| seqs(&[message])[0])
}

async fn open(dir: &tempfile::TempDir) -> Journal {
    Journal::open(dir.path()).await.unwrap()
}

#[tokio::test]
async fn events_wait_for_the_first_sync_to_be_numbered() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;

    assert!(journal.append(segment(1)).is_none());
    assert!(journal.append(incident(40)).is_none());

    assert_eq!(seqs(&journal.anchor(0)), [1, 2]);
    assert_eq!(seq_of(journal.append(segment(2))), Some(3));
}

#[tokio::test]
async fn a_fresh_journal_continues_after_the_server_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.append(segment(1));

    // The server already holds seq 1..=7 from a journal that was lost.
    assert_eq!(seqs(&journal.anchor(7)), [8]);
    assert_eq!(seq_of(journal.append(incident(50))), Some(9));
}

#[tokio::test]
async fn sync_drops_confirmed_events_and_resends_the_rest_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.anchor(0);
    for index in 1..=4 {
        journal.append(segment(index));
    }

    assert_eq!(seqs(&journal.anchor(2)), [3, 4]);
    assert_eq!(seqs(&journal.anchor(4)), Vec::<u64>::new());
}

#[tokio::test]
async fn numbering_never_moves_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.anchor(0);
    journal.append(segment(1));
    journal.append(segment(2));

    // A stale or lower acknowledgement must not reissue seq 1 or 2.
    assert_eq!(seqs(&journal.anchor(0)), [1, 2]);
    assert_eq!(seq_of(journal.append(segment(3))), Some(3));
}

#[tokio::test]
async fn recovered_segments_are_not_queued_twice() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.anchor(0);

    assert_eq!(seq_of(journal.append(segment(1))), Some(1));
    // Still pending: the original entry keeps its sequence number.
    assert!(journal.append(segment(1)).is_none());
    journal.anchor(1);
    // Already confirmed: nothing is sent again.
    assert!(journal.append(segment(1)).is_none());
    assert_eq!(seq_of(journal.append(segment(2))), Some(2));
}

#[tokio::test]
async fn saved_state_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut journal = open(&dir).await;
        journal.anchor(3);
        journal.append(segment(1));
        journal.append(incident(45));
        journal.set_execution(ExecutionState::Recording);
        journal.save().await.unwrap();
    }

    let mut journal = open(&dir).await;
    assert_eq!(journal.execution(), ExecutionState::Recording);
    assert!(journal.append(segment(1)).is_none());
    assert_eq!(seqs(&journal.anchor(3)), [4, 5]);
    assert_eq!(seq_of(journal.append(segment(2))), Some(6));
}

#[tokio::test]
async fn clearing_events_keeps_numbering_and_segment_identity() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir).await;
    journal.anchor(0);
    journal.append(segment(1));
    journal.append(incident(45));

    journal.clear_events();

    assert_eq!(seqs(&journal.anchor(0)), Vec::<u64>::new());
    assert!(journal.append(segment(1)).is_none());
    assert_eq!(seq_of(journal.append(incident(60))), Some(3));
}

#[tokio::test]
async fn a_corrupt_snapshot_is_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = dir.path().join(RUNTIME_DIR);
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(runtime.join(JOURNAL_FILE), b"{ not json").unwrap();

    let mut journal = open(&dir).await;

    assert_eq!(journal.execution(), ExecutionState::Ready);
    assert!(runtime.join(CORRUPT_FILE).exists());
    assert!(journal.append(segment(1)).is_none());
}
