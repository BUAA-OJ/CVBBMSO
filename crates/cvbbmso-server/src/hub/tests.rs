use std::time::Duration;

use axum::extract::ws::Message;
use cvbbmso_protocol::{ClientIncident, ClientMessage, ExamEvent, IncidentCode};
use futures_util::{StreamExt, stream};
use tokio::time;

use super::*;
use crate::socket::read_frames;

#[tokio::test]
async fn heartbeats_bypass_blocked_reliable_writes_and_renew_deadline() {
    let (requests, mut pending) = mpsc::channel(4);
    let (heartbeats, mut pending_hb) = mpsc::channel(4);
    let budget = WriteBudget::new(8, 1024 * 1024);
    let exam = Arc::new(ExamService::open("sqlite::memory:").await.unwrap());
    time::pause();
    let hub = Hub {
        requests,
        heartbeats,
        budget,
        exam,
        config: HubConfig::default(),
    };
    let connection = Connection {
        student: StudentKey {
            exam_id: Uuid::new_v4(),
            oj_user_id: 1,
        },
        id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
    };
    let start = Instant::now();
    let old_deadline = start + Duration::from_secs(1);
    let renewed_deadline = start + Duration::from_secs(25);
    let event = ClientMessage::Event {
        seq: 1,
        event: ExamEvent::Incident(ClientIncident {
            code: IncidentCode::NavigationDenied,
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            detail: None,
        }),
    };
    let event_text = serde_json::to_string(&event).unwrap();
    let heartbeat = serde_json::to_string(&ClientMessage::Heartbeat).unwrap();
    let reader = stream::iter([
        Ok(Message::Text(event_text.into())),
        Ok(Message::Text(heartbeat.into())),
    ])
    .chain(stream::pending());
    tokio::pin!(reader);

    let simulate_hub = async {
        // Receive the reliable event first, holding its completion pending.
        let Some(Request::Reliable {
            message, processed, ..
        }) = pending.recv().await
        else {
            panic!("expected reliable event");
        };
        assert_eq!(message, event);

        // Heartbeat arrives immediately on the fast path while the reliable write is in flight.
        let Some(HeartbeatMsg { ack, .. }) = pending_hb.recv().await else {
            panic!("expected fast-path heartbeat");
        };
        assert!(ack.send(SessionAck::Deadline(renewed_deadline)).is_ok());

        // Advance past old_deadline to verify heartbeat renewed the deadline even while event #1
        // had not finished committing.
        time::advance(Duration::from_secs(2)).await;
        assert!(!processed.is_closed());
        drop(processed);
    };

    time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            read_frames(&mut reader, &hub, connection, old_deadline),
            simulate_hub
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn per_connection_backpressure_pauses_reading_without_heartbeat_timeout() {
    let (requests, mut pending) = mpsc::channel(8);
    let (heartbeats, _pending_hb) = mpsc::channel(4);
    let budget = WriteBudget::new(8, 1024 * 1024);
    let exam = Arc::new(ExamService::open("sqlite::memory:").await.unwrap());
    time::pause();
    let timeout = Duration::from_secs(5);
    let config = HubConfig {
        heartbeat_timeout: timeout,
        max_per_conn_pending: 2,
        ..HubConfig::default()
    };
    let hub = Hub {
        requests,
        heartbeats,
        budget,
        exam,
        config,
    };
    let connection = Connection {
        student: StudentKey {
            exam_id: Uuid::new_v4(),
            oj_user_id: 2,
        },
        id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
    };
    let event = |seq| {
        serde_json::to_string(&ClientMessage::Event {
            seq,
            event: ExamEvent::Incident(ClientIncident {
                code: IncidentCode::NavigationDenied,
                occurred_at: OffsetDateTime::UNIX_EPOCH,
                detail: None,
            }),
        })
        .unwrap()
    };
    let reader = stream::iter([
        Ok(Message::Text(event(1).into())),
        Ok(Message::Text(event(2).into())),
        Ok(Message::Text(event(3).into())),
    ])
    .chain(stream::pending());
    tokio::pin!(reader);

    let simulate_hub = async {
        let mut completions = Vec::new();
        for _ in 0..2 {
            let Some(Request::Reliable { processed, .. }) = pending.recv().await else {
                panic!("expected reliable event");
            };
            completions.push(processed);
        }
        // Wait longer than the heartbeat timeout while the connection is backpressured at cap=2.
        let Some(Request::Backpressure {
            paused: true, ack, ..
        }) = pending.recv().await
        else {
            panic!("expected backpressure pause request");
        };
        let _ = ack.send(SessionAck::Paused);
        time::advance(timeout + Duration::from_secs(2)).await;
        assert!(pending.try_recv().is_err());

        // Complete the first request; backpressure clears and refreshes deadline, allowing #3 in.
        completions
            .remove(0)
            .send(SessionAck::Deadline(Instant::now()))
            .unwrap();
        let Some(Request::Backpressure {
            paused: false, ack, ..
        }) = pending.recv().await
        else {
            panic!("expected backpressure resume request");
        };
        let _ = ack.send(SessionAck::Deadline(Instant::now() + timeout));
        let Some(Request::Reliable {
            message: ClientMessage::Event { seq: 3, .. },
            processed,
            ..
        }) = pending.recv().await
        else {
            panic!("expected third event after backpressure relief");
        };
        drop(completions.pop().unwrap());
        drop(processed);
        drop(pending);
    };

    time::timeout(Duration::from_secs(20), async {
        tokio::join!(
            read_frames(&mut reader, &hub, connection, Instant::now() + timeout),
            simulate_hub
        );
    })
    .await
    .unwrap();
}

async fn setup_test_exam(exam: &ExamService, oj_user_id: i64, start: OffsetDateTime) -> Uuid {
    use cvbbmso_domain::{AdminOperation, AdminOutput};
    use cvbbmso_protocol::RecordingPolicy;

    let AdminOutput::ExamCreated { exam_id } = exam
        .admin()
        .execute(
            "staff".into(),
            AdminOperation::ExamsCreate {
                title: "Hub test exam".into(),
                start_at: start,
                end_at: start + Duration::from_secs(3600),
                policy: RecordingPolicy::new(30, 15).unwrap(),
            },
        )
        .await
        .unwrap()
    else {
        panic!("expected exam");
    };
    exam.admin()
        .execute(
            "staff".into(),
            AdminOperation::ParticipantsAdd {
                exam_id,
                oj_user_id,
            },
        )
        .await
        .unwrap();
    exam_id
}

async fn connect_test_student(
    hub: &Hub,
    student: StudentKey,
) -> (
    Connection,
    mpsc::Receiver<cvbbmso_protocol::ServerMessage>,
    oneshot::Receiver<Option<SocketError>>,
    oneshot::Receiver<Instant>,
) {
    use cvbbmso_protocol::{ClientHello, ClientState, ExecutionState, Platform, RecordingState};

    let connection = Connection {
        student,
        id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
    };
    let (frames_tx, frames_rx) = mpsc::channel(16);
    let (close_tx, close_rx) = oneshot::channel();
    let (admitted_tx, admitted_rx) = oneshot::channel();
    let hello = ClientHello {
        device_id: Uuid::new_v4(),
        instance_id: Uuid::new_v4(),
        platform: Platform::Windows,
        state: ClientState {
            execution: ExecutionState::Ready,
            recording: RecordingState::Idle,
        },
    };
    let peer = Peer {
        frames: frames_tx,
        close: close_tx,
        admitted: admitted_tx,
    };
    assert!(hub.connect(connection, hello, peer).await);
    (connection, frames_rx, close_rx, admitted_rx)
}

#[tokio::test]
async fn backpressure_resume_updates_real_hub_deadline_for_queued_heartbeat() {
    use cvbbmso_domain::{AdminOperation, AdminOutput};
    use cvbbmso_protocol::ServerMessage;
    use tokio::sync::mpsc::unbounded_channel;

    let files = tempfile::tempdir().unwrap();
    let db_path = files.path().join("backpressure.sqlite");
    let exam = Arc::new(
        ExamService::open(&format!("sqlite://{}?mode=rwc", db_path.display()))
            .await
            .unwrap(),
    );
    let start = OffsetDateTime::from_unix_timestamp(2_000_000_000).unwrap();
    let student = StudentKey {
        exam_id: setup_test_exam(&exam, 7, start).await,
        oj_user_id: 7,
    };

    let timeout = Duration::from_millis(300);
    let config = HubConfig {
        heartbeat_timeout: timeout,
        max_per_conn_pending: 2,
        writer: WriterConfig {
            group_commit_delay: Duration::from_millis(450),
            ..WriterConfig::default()
        },
        ..HubConfig::default()
    };
    let (hub, manager) = Manager::with_config(Arc::clone(&exam), config);
    let batch_waiting = Arc::clone(&manager.batch_waiting);
    let manager_task = tokio::spawn(manager.run());

    let (connection, mut frames_rx, mut close_rx, admitted_rx) =
        connect_test_student(&hub, student).await;
    let initial_deadline = admitted_rx.await.unwrap();
    assert!(matches!(
        frames_rx.recv().await,
        Some(ServerMessage::Sync { .. })
    ));

    let (ws_tx, ws_rx) = unbounded_channel::<Result<Message, axum::Error>>();
    let mut reader = Box::pin(futures_util::stream::unfold(ws_rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    let event = |seq| {
        Message::Text(
            serde_json::to_string(&ClientMessage::Event {
                seq,
                event: ExamEvent::Incident(ClientIncident {
                    code: IncidentCode::NavigationDenied,
                    occurred_at: start,
                    detail: None,
                }),
            })
            .unwrap()
            .into(),
        )
    };
    let heartbeat = Message::Text(
        serde_json::to_string(&ClientMessage::Heartbeat)
            .unwrap()
            .into(),
    );
    let reader_hub = hub.clone();
    let reader_task = tokio::spawn(async move {
        read_frames(&mut reader, &reader_hub, connection, initial_deadline).await;
    });

    // Event #1 enters DatabaseWriter (450ms > 300ms timeout); event #2 fills max_per_conn_pending=2.
    let wait1 = batch_waiting.notified();
    tokio::pin!(wait1);
    wait1.as_mut().enable();
    ws_tx.send(Ok(event(1))).unwrap();
    ws_tx.send(Ok(event(2))).unwrap();
    wait1.await;

    // Wait until event #1 commits (>300ms) and event #2 enters DatabaseWriter.
    batch_waiting.notified().await;

    // Backpressure cleared and deadline refreshed; queued heartbeat and event #3 must succeed.
    assert!(ws_tx.send(Ok(heartbeat)).is_ok());
    assert!(ws_tx.send(Ok(event(3))).is_ok());
    hub.barrier().await;

    assert!(close_rx.try_recv().is_err());
    let op = AdminOperation::EventsList {
        exam_id: student.exam_id,
        oj_user_id: student.oj_user_id,
    };
    let AdminOutput::Events(events) = exam.admin().execute("staff".into(), op).await.unwrap()
    else {
        panic!("expected events");
    };
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    ws_tx.send(Ok(Message::Close(None))).unwrap();
    reader_task.await.unwrap();
    manager_task.abort();
}

#[tokio::test]
async fn idle_storage_fault_recovers_on_schedule_tick_and_admits_next_connect() {
    use cvbbmso_protocol::{ClientHello, ClientState, ExecutionState, Platform, RecordingState};

    let files = tempfile::tempdir().unwrap();
    let db_path = files.path().join("backpressure.sqlite");
    let exam = Arc::new(
        ExamService::open(&format!("sqlite://{}?mode=rwc", db_path.display()))
            .await
            .unwrap(),
    );
    let start = OffsetDateTime::from_unix_timestamp(2_000_000_000).unwrap();
    let exam_id = setup_test_exam(&exam, 1, start).await;

    let (hub, mut manager) = Manager::new(Arc::clone(&exam), Duration::from_secs(25));
    let recovered = Arc::clone(&manager.storage_recovered);
    let storage_err = DomainError::from(sea_orm::DbErr::ConnectionAcquire(
        sea_orm::ConnAcquireErr::Timeout,
    ));
    assert!(storage_err.is_storage_fault());
    manager.handle_batch_completion(WriteCompletion::Batch {
        ops: Vec::new(),
        heartbeat_batches: Vec::new(),
        probed: false,
        batch_error: Some(Arc::new(storage_err)),
    });
    assert_eq!(manager.storage, StorageHealth::Faulted);
    let manager_task = tokio::spawn(manager.run());

    let student = StudentKey {
        exam_id,
        oj_user_id: 1,
    };
    let hello = || ClientHello {
        device_id: Uuid::new_v4(),
        instance_id: Uuid::new_v4(),
        platform: Platform::Windows,
        state: ClientState {
            execution: ExecutionState::Ready,
            recording: RecordingState::Idle,
        },
    };

    // First admission while storage_fault is active is rejected.
    let (frames_tx, _frames_rx) = mpsc::channel(4);
    let (close_tx, close_rx) = oneshot::channel();
    let (admitted_tx, _admitted_rx) = oneshot::channel();
    assert!(
        hub.connect(
            Connection {
                student,
                id: Uuid::new_v4(),
                request_id: Uuid::new_v4(),
            },
            hello(),
            Peer {
                frames: frames_tx,
                close: close_tx,
                admitted: admitted_tx,
            },
        )
        .await
    );
    assert!(close_rx.await.unwrap().is_some());

    // An idle schedule tick probes storage and clears storage_fault.
    let notified = recovered.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    hub.trigger_schedule().await;
    notified.await;

    let (frames_tx, mut frames_rx) = mpsc::channel(4);
    let (close_tx, _close_rx) = oneshot::channel();
    let (admitted_tx, admitted_rx) = oneshot::channel();
    assert!(
        hub.connect(
            Connection {
                student,
                id: Uuid::new_v4(),
                request_id: Uuid::new_v4(),
            },
            hello(),
            Peer {
                frames: frames_tx,
                close: close_tx,
                admitted: admitted_tx,
            },
        )
        .await
    );
    admitted_rx.await.unwrap();
    assert!(matches!(
        frames_rx.recv().await,
        Some(ServerMessage::Sync { .. })
    ));

    manager_task.abort();
}

#[tokio::test]
async fn unrelated_batch_completion_preserves_in_flight_heartbeat_snapshot() {
    let exam = Arc::new(ExamService::open("sqlite::memory:").await.unwrap());
    let (_hub, mut manager) = Manager::new(exam, Duration::from_secs(25));
    let student = StudentKey {
        exam_id: Uuid::new_v4(),
        oj_user_id: 9,
    };
    let conn_id = Uuid::new_v4();
    let (frames_tx, _frames_rx) = mpsc::channel(4);
    let (close_tx, _close_rx) = oneshot::channel();
    manager.students.insert(
        student,
        StudentSlot {
            current: Some(Current {
                connection: Connection {
                    student,
                    id: conn_id,
                    request_id: Uuid::new_v4(),
                },
                deadline: Instant::now() + Duration::from_secs(25),
                backpressure_paused: false,
                frames: frames_tx,
                close: close_tx,
                state: cvbbmso_protocol::ClientState {
                    execution: cvbbmso_protocol::ExecutionState::Ready,
                    recording: cvbbmso_protocol::RecordingState::Idle,
                },
                exam_times: None,
                start: None,
                end: None,
            }),
            ..StudentSlot::default()
        },
    );

    let active_batch_id = Uuid::new_v4();
    let mut snapshot = HashMap::new();
    snapshot.insert(
        student,
        HeartbeatEntry {
            connection_id: conn_id,
            last_seen_at: OffsetDateTime::UNIX_EPOCH,
        },
    );
    manager.heartbeats.in_flight = Some((active_batch_id, snapshot));

    // Unrelated batch completion must leave in_flight intact.
    manager.handle_batch_completion(WriteCompletion::Batch {
        ops: Vec::new(),
        heartbeat_batches: vec![Uuid::new_v4()],
        probed: false,
        batch_error: None,
    });
    assert!(manager.heartbeats.in_flight.is_some());

    // Actual heartbeat batch failure must recover the snapshot back into dirty.
    manager.handle_batch_completion(WriteCompletion::Batch {
        ops: Vec::new(),
        heartbeat_batches: vec![active_batch_id],
        probed: false,
        batch_error: Some(Arc::new(DomainError::InvalidInput)),
    });
    assert!(manager.heartbeats.in_flight.is_none());
    assert!(manager.heartbeats.dirty.contains_key(&student));
}

#[tokio::test]
async fn busy_storage_probe_requeues_and_recovers_after_unlock() {
    use sea_orm::{
        ConnectionTrait, Database, SqliteTransactionMode, TransactionOptions, TransactionTrait,
    };

    let files = tempfile::tempdir().unwrap();
    let db_path = files.path().join("probe_busy.sqlite");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let exam = Arc::new(ExamService::open(&url).await.unwrap());
    let start = OffsetDateTime::from_unix_timestamp(2_000_000_000).unwrap();
    let student = StudentKey {
        exam_id: setup_test_exam(&exam, 1, start).await,
        oj_user_id: 1,
    };

    let (hub, mut manager) = Manager::new(Arc::clone(&exam), Duration::from_secs(25));
    let recovered = Arc::clone(&manager.storage_recovered);
    manager.storage = StorageHealth::Faulted;
    let manager_task = tokio::spawn(manager.run());

    // Hold an exclusive SQLite write lock so the first storage probe fails with SQLITE_BUSY.
    let locker = Database::connect(&url).await.unwrap();
    let lock_tx = locker
        .begin_with_options(TransactionOptions {
            sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = lock_tx.execute_unprepared("SELECT 1").await.unwrap();

    hub.trigger_schedule().await;
    time::sleep(Duration::from_millis(350)).await;
    hub.barrier().await;

    // Admission must still be rejected while locked.
    let (_, _, close_rx, _) = connect_test_student(&hub, student).await;
    assert!(close_rx.await.unwrap().is_some());

    // Release the lock and verify the next schedule tick probes again and recovers admissions.
    lock_tx.rollback().await.unwrap();
    let notified = recovered.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    hub.trigger_schedule().await;
    notified.await;

    let (_, mut frames_rx, _, admitted_rx) = connect_test_student(&hub, student).await;
    admitted_rx.await.unwrap();
    assert!(matches!(
        frames_rx.recv().await,
        Some(cvbbmso_protocol::ServerMessage::Sync { .. })
    ));

    manager_task.abort();
}

fn make_test_current(
    connection: Connection,
    deadline: Instant,
    backpressure_paused: bool,
) -> (
    Current,
    mpsc::Receiver<cvbbmso_protocol::ServerMessage>,
    oneshot::Receiver<Option<SocketError>>,
) {
    let (frames_tx, frames_rx) = mpsc::channel(4);
    let (close_tx, close_rx) = oneshot::channel();
    let current = Current {
        connection,
        deadline,
        backpressure_paused,
        frames: frames_tx,
        close: close_tx,
        state: cvbbmso_protocol::ClientState {
            execution: cvbbmso_protocol::ExecutionState::Ready,
            recording: cvbbmso_protocol::RecordingState::Idle,
        },
        exam_times: None,
        start: None,
        end: None,
    };
    (current, frames_rx, close_rx)
}

#[tokio::test]
async fn deferred_command_ack_rechecks_and_delivers_after_failed_removal() {
    use cvbbmso_protocol::{
        ClientHello, ClientState, ExecutionState, Platform, RecordingState, ServerMessage,
    };

    let files = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        files.path().join("deferred_ack.sqlite").display()
    );
    let exam = Arc::new(ExamService::open(&url).await.unwrap());
    let start = OffsetDateTime::from_unix_timestamp(2_000_000_000).unwrap();
    let student = StudentKey {
        exam_id: setup_test_exam(&exam, 11, start).await,
        oj_user_id: 11,
    };
    let conn_id = Uuid::new_v4();
    let connection = Connection {
        student,
        id: conn_id,
        request_id: Uuid::new_v4(),
    };
    let state = ClientState {
        execution: ExecutionState::Ready,
        recording: RecordingState::Idle,
    };
    let hello = ClientHello {
        device_id: Uuid::new_v4(),
        instance_id: Uuid::new_v4(),
        platform: Platform::Windows,
        state,
    };
    exam.execute_batch(
        vec![BatchWrite::Connect {
            exam_id: student.exam_id,
            oj_user_id: student.oj_user_id,
            connection_id: conn_id,
            hello,
            observed_at: start,
        }],
        Vec::new(),
    )
    .await
    .unwrap();

    let (hub, mut manager) = Manager::new(Arc::clone(&exam), Duration::from_secs(25));
    let op_id = Uuid::new_v4();
    let (current, mut frames_rx, _close_rx) =
        make_test_current(connection, Instant::now() + Duration::from_secs(25), false);
    manager.students.insert(
        student,
        StudentSlot {
            current: Some(current),
            executing: Some(op_id),
            barrier: true,
            ..StudentSlot::default()
        },
    );
    let (reply_tx, reply_rx) = oneshot::channel();
    let kind = InFlightKind::AdminRemove { reply: reply_tx };
    manager.in_flight_ops.insert(
        op_id,
        InFlightOp {
            operation_id: op_id,
            student,
            kind,
        },
    );

    let command_id = Uuid::new_v4();
    let permit = hub.budget().try_acquire(64).unwrap();
    let (processed_tx, mut processed_rx) = oneshot::channel();
    manager.handle_read_check(ReadCheckCompletion {
        connection,
        command_id,
        result: Ok(()),
        permit,
        processed: processed_tx,
    });
    assert!(frames_rx.try_recv().is_err());
    assert!(processed_rx.try_recv().is_err());

    manager.handle_batch_completion(WriteCompletion::Batch {
        ops: vec![(op_id, Some(Err(DomainError::Conflict)))],
        heartbeat_batches: Vec::new(),
        probed: false,
        batch_error: None,
    });
    assert!(matches!(
        reply_rx.await.unwrap(),
        Err(DomainError::Conflict)
    ));

    let rechecked = manager.read_checks_rx.recv().await.unwrap();
    manager.handle_read_check(rechecked);
    assert_eq!(
        frames_rx.try_recv().unwrap(),
        ServerMessage::CommandAck { command_id }
    );
    assert!(matches!(
        processed_rx.try_recv().unwrap(),
        SessionAck::Deadline(_)
    ));
}

#[tokio::test]
async fn backpressure_pause_preserves_busy_retry_delay_past_old_deadline() {
    use sea_orm::{
        ConnectionTrait, Database, SqliteTransactionMode, TransactionOptions, TransactionTrait,
    };

    let files = tempfile::tempdir().unwrap();
    let db_path = files.path().join("retry_delay.sqlite");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let exam = Arc::new(ExamService::open(&url).await.unwrap());

    // Capture a real SQLITE_BUSY DomainError.
    let locker = Database::connect(&url).await.unwrap();
    let lock_tx = locker
        .begin_with_options(TransactionOptions {
            sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = lock_tx.execute_unprepared("SELECT 1").await.unwrap();
    let busy_err = exam.probe_storage().await.unwrap_err();
    assert!(busy_err.is_storage_busy());
    lock_tx.rollback().await.unwrap();

    time::pause();
    let (hub, mut manager) = Manager::new(exam, Duration::from_secs(25));
    let student = StudentKey {
        exam_id: Uuid::new_v4(),
        oj_user_id: 21,
    };
    let connection = Connection {
        student,
        id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
    };
    let old_deadline = Instant::now();
    time::advance(Duration::from_secs(2)).await;
    let now = Instant::now();
    assert!(now > old_deadline);

    let (frames_tx, _frames_rx) = mpsc::channel(4);
    let (close_tx, _close_rx) = oneshot::channel();
    manager.students.insert(
        student,
        StudentSlot {
            current: Some(Current {
                connection,
                deadline: old_deadline,
                backpressure_paused: true,
                frames: frames_tx,
                close: close_tx,
                state: cvbbmso_protocol::ClientState {
                    execution: cvbbmso_protocol::ExecutionState::Ready,
                    recording: cvbbmso_protocol::RecordingState::Idle,
                },
                exam_times: None,
                start: None,
                end: None,
            }),
            ..StudentSlot::default()
        },
    );

    let permit = hub.budget().try_acquire(64).unwrap();
    let (processed_tx, _processed_rx) = oneshot::channel();
    manager.handle_reliable_error(
        student,
        connection,
        ReliableOp::Status(cvbbmso_protocol::ClientState {
            execution: cvbbmso_protocol::ExecutionState::Ready,
            recording: cvbbmso_protocol::RecordingState::Idle,
        }),
        OffsetDateTime::UNIX_EPOCH,
        1,
        permit,
        processed_tx,
        &busy_err,
    );

    let (&(retry_at, _), _) = manager
        .retry_timers
        .first_key_value()
        .expect("expected scheduled busy retry");
    assert!(retry_at >= now + WRITE_RETRY_DELAY);
}
