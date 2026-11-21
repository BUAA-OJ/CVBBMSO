//! Reusable acceptance scenarios, including documented recovery limitations.

use cvbbmso_domain::{DisconnectReason, StudentRuntime};
use sea_orm::{ConnectionTrait, Database, TransactionTrait};
use tokio::time::Instant;

use super::*;

async fn runtime(fixture: &Fixture, user: i64) -> StudentRuntime {
    let AdminOutput::Student(runtime) = fixture
        .admin(AdminOperation::StudentsShow {
            exam_id: fixture.exam_id,
            oj_user_id: user,
        })
        .await
    else {
        panic!("expected runtime")
    };
    runtime
}

#[tokio::test]
async fn same_session_status_regression_is_rejected_and_reconnect_restores_recording() {
    for (from, to) in [
        (
            ClientState {
                execution: ExecutionState::Recording,
                recording: RecordingState::Capturing,
            },
            ClientState {
                execution: ExecutionState::Ready,
                recording: RecordingState::Idle,
            },
        ),
        (
            ClientState {
                execution: ExecutionState::Finalizing,
                recording: RecordingState::Finalizing,
            },
            ClientState {
                execution: ExecutionState::Ready,
                recording: RecordingState::Idle,
            },
        ),
        (
            ClientState {
                execution: ExecutionState::Finalizing,
                recording: RecordingState::Failed,
            },
            ClientState {
                execution: ExecutionState::Recording,
                recording: RecordingState::Failed,
            },
        ),
    ] {
        let fixture = Fixture::new().await;
        fixture.clock.store(START, Ordering::Relaxed);
        let state = hello(ExecutionState::Ready, RecordingState::Idle);
        let mut socket = open_socket(&fixture, &state, 0).await;
        let old_start = command(&mut socket, CommandName::StartExam, START).await;
        confirm(&mut socket, old_start).await;
        send(&mut socket, &ClientMessage::Status { state: from }).await;
        send(&mut socket, &navigation(1, "before invalid status")).await;
        send(&mut socket, &ClientMessage::Status { state: to }).await;
        socket_error(&mut socket, error_code::PROTOCOL_ERROR).await;
        fixture.state.hub.barrier().await;
        let previous = runtime(&fixture, STUDENT).await;
        assert!(!previous.connected);
        assert_eq!(previous.execution, from.execution);
        assert_eq!(events(&fixture).await.len(), 1);

        let mut resumed = open_socket(&fixture, &state, 1).await;
        let new_start = command(&mut resumed, CommandName::StartExam, START).await;
        assert_ne!(old_start, new_start);
        confirm(&mut resumed, new_start).await;
        confirm(&mut resumed, new_start).await;
        assert_eq!(
            runtime(&fixture, STUDENT).await.execution,
            ExecutionState::Recording
        );
        fixture.clock.store(END, Ordering::Relaxed);
        fixture.state.hub.trigger_schedule().await;
        command(&mut resumed, CommandName::EndExam, END).await;
        close_socket(&fixture, &mut resumed).await;
    }
}

#[tokio::test]
async fn runtime_loss_after_end_stays_ready_until_explicit_finalizing_reconnect() {
    let fixture = Fixture::new().await;
    fixture.clock.store(START, Ordering::Relaxed);
    let mut peer = TestPeer::connect(
        &fixture,
        STUDENT,
        ExecutionState::Ready,
        RecordingState::Idle,
        0,
    )
    .await;
    let start = peer.command(CommandName::StartExam, START).await;
    peer.result(&fixture, start, CommandStatus::Completed).await;
    assert!(
        peer.receive(&fixture, navigation(1, "before runtime loss"))
            .await
    );
    fixture
        .state
        .hub
        .end(peer.connection, DisconnectReason::Closed, None)
        .await;
    fixture.state.hub.barrier().await;
    assert_eq!(
        runtime(&fixture, STUDENT).await.execution,
        ExecutionState::Recording
    );

    fixture.clock.store(END, Ordering::Relaxed);
    let mut resumed = TestPeer::connect(
        &fixture,
        STUDENT,
        ExecutionState::Ready,
        RecordingState::Idle,
        1,
    )
    .await;
    for _ in 0..2 {
        assert!(resumed.receive(&fixture, ClientMessage::Heartbeat).await);
        fixture.state.hub.trigger_schedule().await;
        resumed.no_output(&fixture).await;
    }
    let current = runtime(&fixture, STUDENT).await;
    assert!(current.connected);
    assert_eq!(current.execution, ExecutionState::Ready);
    assert_eq!(events(&fixture).await.len(), 1);
    let mut kinds = incidents(&fixture)
        .await
        .into_iter()
        .map(|item| item.kind)
        .collect::<Vec<_>>();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            "connection_lost",
            "connection_restored",
            "navigation_denied",
            "process_restarted"
        ]
    );

    // This is a protocol-level recovery probe, not automatic client recovery.
    let mut finalizing = TestPeer::connect(
        &fixture,
        STUDENT,
        ExecutionState::Finalizing,
        RecordingState::Failed,
        1,
    )
    .await;
    let end = finalizing.command(CommandName::EndExam, END).await;
    finalizing
        .result(&fixture, end, CommandStatus::Failed)
        .await;
    assert_eq!(
        runtime(&fixture, STUDENT).await.execution,
        ExecutionState::Finished
    );
}

#[tokio::test]
async fn permanent_event_gap_rejects_failed_end_across_reconnects() {
    let fixture = Fixture::new().await;
    fixture.clock.store(END, Ordering::Relaxed);
    let state = hello(ExecutionState::Finalizing, RecordingState::Failed);
    let mut socket = open_socket(&fixture, &state, 0).await;
    let first_end = command(&mut socket, CommandName::EndExam, END).await;
    send(&mut socket, &navigation(1, "persisted")).await;
    send(&mut socket, &navigation(3, "after missing event")).await;
    send(&mut socket, &result(first_end, CommandStatus::Failed)).await;
    socket_error(&mut socket, error_code::COMMAND_STATE_CONFLICT).await;
    fixture.state.hub.barrier().await;
    let mut last_end = first_end;
    for _ in 0..2 {
        let mut socket = open_socket(&fixture, &state, 1).await;
        let end = command(&mut socket, CommandName::EndExam, END).await;
        assert_ne!(end, last_end);
        send(&mut socket, &result(end, CommandStatus::Failed)).await;
        socket_error(&mut socket, error_code::COMMAND_STATE_CONFLICT).await;
        fixture.state.hub.barrier().await;
        let current = runtime(&fixture, STUDENT).await;
        assert!(!current.connected);
        assert_eq!(current.execution, ExecutionState::Finalizing);
        assert_eq!(
            events(&fixture)
                .await
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        last_end = end;
    }
    // Restoring the missing event is the control: failure can then be committed.
    let mut socket = open_socket(&fixture, &state, 1).await;
    let end = command(&mut socket, CommandName::EndExam, END).await;
    send(&mut socket, &navigation(2, "restored missing event")).await;
    send(&mut socket, &result(end, CommandStatus::Failed)).await;
    assert_eq!(
        message(&mut socket).await,
        ServerMessage::CommandAck { command_id: end }
    );
    close_socket(&fixture, &mut socket).await;
    let current = runtime(&fixture, STUDENT).await;
    assert!(!current.connected);
    assert_eq!(current.execution, ExecutionState::Finished);
    assert_eq!(
        events(&fixture)
            .await
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let finished = hello(ExecutionState::Finished, RecordingState::Failed);
    let mut socket = open_socket(&fixture, &finished, 3).await;
    close_socket(&fixture, &mut socket).await;
}

async fn assert_replacement_evidence(fixture: &Fixture, order: &str) {
    let evidence = incidents(fixture).await;
    if order == "result_first" {
        assert!(evidence.is_empty());
        return;
    }
    assert_eq!(evidence.len(), 3);
    let lost = evidence
        .iter()
        .find(|item| item.kind == "connection_lost")
        .unwrap();
    assert_eq!(
        lost.detail.as_deref(),
        Some(if order == "timeout_first" {
            "heartbeat_timeout"
        } else {
            "replaced"
        })
    );
    for kind in ["connection_restored", "process_restarted"] {
        assert_eq!(evidence.iter().filter(|item| item.kind == kind).count(), 1);
    }
}

#[tokio::test]
async fn replacement_timeout_and_end_result_interleavings_preserve_current_session() {
    // Explicit orders cover both sides of the state transitions without races.
    for order in ["result_first", "replacement_first", "timeout_first"] {
        let fixture = Fixture::new().await;
        let hub = &fixture.state.hub;
        fixture.clock.store(END, Ordering::Relaxed);
        let mut previous = TestPeer::connect(
            &fixture,
            STUDENT,
            ExecutionState::Finalizing,
            RecordingState::Failed,
            0,
        )
        .await;
        let old_end = previous.command(CommandName::EndExam, END).await;
        if order == "result_first" {
            previous
                .result(&fixture, old_end, CommandStatus::Failed)
                .await;
        }
        // SQLite uses a real worker thread. Keep the clock running during I/O
        // so Tokio cannot auto-advance past the pool's acquisition deadline.
        tokio::time::pause();
        tokio::time::advance(TIMEOUT).await;
        tokio::time::resume();
        if order == "timeout_first" {
            hub.end(
                previous.connection,
                DisconnectReason::HeartbeatTimeout,
                None,
            )
            .await;
            hub.barrier().await;
        }
        let mut current = TestPeer::connect(
            &fixture,
            STUDENT,
            ExecutionState::Finalizing,
            RecordingState::Failed,
            0,
        )
        .await;
        let new_end = current.command(CommandName::EndExam, END).await;
        assert_ne!(old_end, new_end);
        assert!(previous.closing.await.unwrap().is_none());
        hub.end(
            previous.connection,
            DisconnectReason::HeartbeatTimeout,
            None,
        )
        .await;
        hub.end(previous.connection, DisconnectReason::Closed, None)
            .await;
        assert!(
            hub.receive(
                previous.connection,
                result(old_end, CommandStatus::Failed),
                Instant::now(),
            )
            .await
            .is_none()
        );
        assert!(
            hub.receive(previous.connection, navigation(1, "stale"), Instant::now())
                .await
                .is_none()
        );
        hub.trigger_schedule().await;
        current.no_output(&fixture).await;
        let before = runtime(&fixture, STUDENT).await;
        assert!(before.connected);
        assert_eq!(
            before.execution,
            if order == "result_first" {
                ExecutionState::Finished
            } else {
                ExecutionState::Finalizing
            }
        );
        assert!(events(&fixture).await.is_empty());
        current
            .result(&fixture, new_end, CommandStatus::Failed)
            .await;
        current
            .result(&fixture, new_end, CommandStatus::Failed)
            .await;
        hub.end(current.connection, DisconnectReason::Closed, None)
            .await;
        hub.barrier().await;
        let after = runtime(&fixture, STUDENT).await;
        assert!(!after.connected);
        assert_eq!(after.execution, ExecutionState::Finished);
        assert_replacement_evidence(&fixture, order).await;
    }
}

#[tokio::test]
async fn sqlite_retry_allows_other_student_read_only_ack_then_recovers_writes() {
    let fixture = Fixture::new().await;
    fixture
        .admin(AdminOperation::ParticipantsAdd {
            exam_id: fixture.exam_id,
            oj_user_id: OTHER_STUDENT,
        })
        .await;
    fixture.clock.store(START, Ordering::Relaxed);
    let first = TestPeer::connect(
        &fixture,
        STUDENT,
        ExecutionState::Ready,
        RecordingState::Idle,
        0,
    )
    .await;
    let mut other = TestPeer::connect(
        &fixture,
        OTHER_STUDENT,
        ExecutionState::Ready,
        RecordingState::Idle,
        0,
    )
    .await;
    let start = other.command(CommandName::StartExam, START).await;
    other
        .result(&fixture, start, CommandStatus::Completed)
        .await;

    let writer = Database::connect(&fixture.database_url).await.unwrap();
    let lock = writer.begin().await.unwrap();
    lock.execute_unprepared("UPDATE exams SET title = title")
        .await
        .unwrap();
    let hub = fixture.state.hub.clone();
    let connection = first.connection;
    let write = tokio::spawn(async move {
        hub.receive(connection, navigation(1, "retried write"), Instant::now())
            .await
    });
    tokio::time::timeout(TIMEOUT, fixture.write_retries.notified())
        .await
        .unwrap();
    assert!(!write.is_finished());

    // A global SQLite writer lock blocks all new writes. A committed result's
    // replay only reads, and must still get its ACK while that lock is held.
    tokio::time::timeout(
        TIMEOUT,
        other.result(&fixture, start, CommandStatus::Completed),
    )
    .await
    .unwrap();
    assert!(!write.is_finished());
    assert!(runtime(&fixture, OTHER_STUDENT).await.connected);
    lock.rollback().await.unwrap();
    assert!(
        tokio::time::timeout(TIMEOUT, write)
            .await
            .unwrap()
            .unwrap()
            .is_some()
    );
    assert!(
        other
            .receive(&fixture, navigation(1, "independent student"))
            .await
    );
    assert!(other.receive(&fixture, ClientMessage::Heartbeat).await);
    assert_eq!(events(&fixture).await.len(), 1);
    let AdminOutput::Events(other_events) = fixture
        .admin(AdminOperation::EventsList {
            exam_id: fixture.exam_id,
            oj_user_id: OTHER_STUDENT,
        })
        .await
    else {
        panic!("expected events")
    };
    assert_eq!(other_events.len(), 1);
    assert_eq!(other_events[0].seq, 1);
    assert_ne!(other_events[0].event, events(&fixture).await[0].event);
    for user in [STUDENT, OTHER_STUDENT] {
        assert!(runtime(&fixture, user).await.connected);
    }
    assert!(
        incidents(&fixture)
            .await
            .iter()
            .all(|item| item.kind != "connection_lost")
    );
}
