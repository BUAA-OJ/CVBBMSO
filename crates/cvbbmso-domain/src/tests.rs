use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

use cvbbmso_protocol::{
    ClientHello, ClientIncident, ClientMessage, ClientState, CommandName, CommandStatus, ExamEvent,
    ExamPhase, ExecutionState, IncidentCode, Platform, RecordingPolicy, RecordingState,
    RecordingSummary, ServerMessage, Sha256Digest, error_code,
};
use sea_orm::{ConnectionTrait, EntityTrait, PaginatorTrait, Set};
use time::{Duration, OffsetDateTime, UtcOffset};
use uuid::Uuid;

use crate::{
    AdminOperation, AdminOutput, AdminService, ConnectOutcome, DisconnectReason, DomainError,
    ExamService, IncidentRecord, StudentRuntime, db,
    entity::{audit, command as stored_command, exam as stored_exam},
};

const USER: i64 = 7;
const START: i64 = 1_800_000_000_000_000_000;
const END: i64 = START + 3_600_000_000_000;

struct Fixture {
    exam: Uuid,
    service: ExamService,
    clock: Arc<AtomicI64>,
}

impl Fixture {
    async fn open(url: &str) -> Self {
        let clock = Arc::new(AtomicI64::new(START - 1));
        let clock_source = Arc::clone(&clock);
        let service =
            ExamService::open_with_clock(url, move || time(clock_source.load(Ordering::Relaxed)))
                .await
                .unwrap();
        let AdminOutput::ExamCreated { exam_id } = service
            .admin()
            .execute(
                "tester".into(),
                AdminOperation::ExamsCreate {
                    title: "Programming".into(),
                    start_at: time(START),
                    end_at: time(END),
                    policy: RecordingPolicy::new(30, 15).unwrap(),
                },
            )
            .await
            .unwrap()
        else {
            panic!("exam was not created")
        };
        let fixture = Self {
            exam: exam_id,
            service,
            clock,
        };
        fixture
            .admin(AdminOperation::ParticipantsAdd {
                exam_id,
                oj_user_id: USER,
            })
            .await;
        fixture
    }

    async fn memory() -> Self {
        Self::open("sqlite::memory:").await
    }

    fn set(&self, nanos: i64) {
        self.clock.store(nanos, Ordering::Relaxed);
    }

    async fn admin(&self, op: AdminOperation) -> AdminOutput {
        self.service
            .admin()
            .execute("tester".into(), op)
            .await
            .unwrap()
    }

    async fn connect(&self, hello: ClientHello) -> ConnectOutcome {
        self.service.connect(self.exam, USER, hello).await.unwrap()
    }

    async fn receive(&self, message: ClientMessage) {
        self.service
            .receive(self.exam, USER, message)
            .await
            .unwrap();
    }

    async fn report(
        &self,
        command_id: Uuid,
        name: CommandName,
        status: CommandStatus,
        error_code: Option<&str>,
    ) -> Result<(), DomainError> {
        self.service
            .record_command_result(
                self.exam,
                USER,
                command_id,
                name,
                status,
                error_code.map(str::to_owned),
            )
            .await
    }

    async fn runtime(&self) -> StudentRuntime {
        let AdminOutput::Student(runtime) = self
            .admin(AdminOperation::StudentsShow {
                exam_id: self.exam,
                oj_user_id: USER,
            })
            .await
        else {
            panic!("expected runtime")
        };
        runtime
    }

    async fn incidents(&self) -> Vec<IncidentRecord> {
        let AdminOutput::Incidents(incidents) = self
            .admin(AdminOperation::IncidentsList {
                exam_id: self.exam,
                oj_user_id: Some(USER),
                kind: None,
                since: None,
                until: None,
            })
            .await
        else {
            panic!("expected incidents")
        };
        incidents
    }

    // SeaQuery does not build SQLite triggers. These inject database failures
    // at specific writes so the tests can check transaction rollback.
    async fn trigger_ddl(&self, sql: &str) {
        self.service
            .admin()
            .0
            .writer
            .execute_unprepared(sql)
            .await
            .unwrap();
    }
}

fn time(nanos: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos)).unwrap()
}

fn state(execution: ExecutionState, recording: RecordingState) -> ClientState {
    ClientState {
        execution,
        recording,
    }
}

fn hello(execution: ExecutionState, recording: RecordingState) -> ClientHello {
    ClientHello {
        device_id: Uuid::new_v4(),
        instance_id: Uuid::new_v4(),
        platform: Platform::Windows,
        state: state(execution, recording),
    }
}

fn ready() -> ClientHello {
    hello(ExecutionState::Ready, RecordingState::Idle)
}

fn command(message: &ServerMessage, expected_name: CommandName, expected_time: i64) -> Uuid {
    let ServerMessage::Command {
        name,
        command_id,
        effective_at,
    } = *message
    else {
        panic!("expected a command")
    };
    assert_eq!(name, expected_name);
    assert_eq!(effective_at, time(expected_time));
    command_id
}

fn sync(outcome: &ConnectOutcome, expected_phase: ExamPhase, expected_ack: i64) {
    let ServerMessage::Sync {
        phase,
        ack_event_through,
        ..
    } = outcome.sync
    else {
        panic!("expected a sync")
    };
    assert_eq!(phase, expected_phase);
    assert_eq!(ack_event_through, expected_ack);
}

fn incident_event(seq: i64, timestamp: OffsetDateTime) -> ClientMessage {
    ClientMessage::Event {
        seq,
        event: ExamEvent::Incident(ClientIncident {
            code: IncidentCode::NavigationDenied,
            occurred_at: timestamp,
            detail: None,
        }),
    }
}

fn recording_event(seq: i64, hash: char) -> (ClientMessage, RecordingSummary) {
    let summary = RecordingSummary::new(
        1,
        Sha256Digest::parse(&hash.to_string().repeat(64)).unwrap(),
    )
    .unwrap();
    (
        ClientMessage::Event {
            seq,
            event: ExamEvent::RecordingFinalized(summary.clone()),
        },
        summary,
    )
}

#[tokio::test]
async fn enrollment_is_rechecked_and_removal_preserves_prior_evidence() {
    let f = Fixture::memory().await;
    assert!(matches!(
        f.service.check_enrollment(Uuid::nil(), USER + 1).await,
        Err(DomainError::NotFound)
    ));
    assert!(matches!(
        f.service.check_enrollment(f.exam, USER + 1).await,
        Err(DomainError::NotEnrolled)
    ));
    assert_eq!(f.service.list_for(USER + 1).await.unwrap(), []);
    f.service.check_enrollment(f.exam, USER).await.unwrap();
    let hello = ready();
    f.connect(hello.clone()).await;
    f.receive(incident_event(1, time(START - 1))).await;
    f.admin(AdminOperation::ParticipantsRemove {
        exam_id: f.exam,
        oj_user_id: USER,
    })
    .await;
    assert!(matches!(
        f.service.connect(f.exam, USER, hello.clone()).await,
        Err(DomainError::NotEnrolled)
    ));
    assert!(!f.runtime().await.connected);
    assert!(matches!(
        f.service
            .receive(f.exam, USER, incident_event(2, time(START - 1)))
            .await,
        Err(DomainError::NotEnrolled)
    ));
    assert_eq!(
        f.incidents()
            .await
            .iter()
            .filter(|incident| incident.kind == "navigation_denied")
            .count(),
        1
    );
    f.set(START);
    assert!(f.service.due_commands().await.unwrap().is_empty());
    f.admin(AdminOperation::ParticipantsAdd {
        exam_id: f.exam,
        oj_user_id: USER,
    })
    .await;
    assert!(matches!(
        f.service
            .receive(f.exam, USER, ClientMessage::Heartbeat)
            .await,
        Err(DomainError::NotEnrolled)
    ));
    sync(&f.connect(hello).await, ExamPhase::Running, 1);
}

#[tokio::test]
async fn participant_import_counts_only_new_enrollments() {
    let f = Fixture::memory().await;
    let duplicate = AdminOperation::ParticipantsAdd {
        exam_id: f.exam,
        oj_user_id: USER,
    };
    assert!(matches!(
        f.service.admin().execute("tester".into(), duplicate).await,
        Err(DomainError::Conflict)
    ));

    let import = AdminOperation::ParticipantsImport {
        exam_id: f.exam,
        oj_user_ids: vec![USER + 1, USER, USER + 1, USER + 2],
    };
    assert!(matches!(
        f.admin(import.clone()).await,
        AdminOutput::ParticipantsImported { count: 2 }
    ));
    assert!(matches!(
        f.admin(import).await,
        AdminOutput::ParticipantsImported { count: 0 }
    ));
    for user in [USER, USER + 1, USER + 2] {
        f.service.check_enrollment(f.exam, user).await.unwrap();
    }
    let rows = audit::Entity::find()
        .all(&f.service.admin().0.writer)
        .await
        .unwrap();
    let operations = rows
        .iter()
        .map(|row| db::decode::<serde_json::Value>(&row.operation).unwrap());
    assert_eq!(
        operations
            .filter(|op| op["operation"] == "participants_import")
            .collect::<Vec<_>>(),
        vec![
            serde_json::json!({
                "operation": "participants_import",
                "exam_id": f.exam,
                "oj_user_ids": [USER + 1, USER, USER + 1, USER + 2]
            });
            2
        ]
    );
}

#[tokio::test]
async fn enrollment_removal_is_rejected_at_and_after_start() {
    let f = Fixture::memory().await;
    let op = AdminOperation::ParticipantsRemove {
        exam_id: f.exam,
        oj_user_id: USER,
    };
    for now in [START, START + 1, END] {
        f.set(now);
        assert!(matches!(
            f.service.admin().execute("tester".into(), op.clone()).await,
            Err(DomainError::Conflict)
        ));
        f.service.check_enrollment(f.exam, USER).await.unwrap();
    }
}

#[tokio::test]
async fn time_boundaries_determine_command_candidates() {
    let f = Fixture::memory().await;
    let hello = ready();
    let outcome = f.connect(hello).await;
    sync(&outcome, ExamPhase::Scheduled, 0);
    assert_eq!(outcome.commands, []);

    let cases = [
        (START - 1, ExamPhase::Scheduled),
        (START, ExamPhase::Running),
        (END - 1, ExamPhase::Running),
        (END, ExamPhase::Ended),
    ];
    for (now, phase) in cases {
        f.set(now);
        assert_eq!(f.service.list_for(USER).await.unwrap()[0].phase, phase);
        let commands = f.service.due_commands().await.unwrap();
        match now {
            START => {
                assert_eq!(commands.len(), 1);
                let command_id = command(&commands[0].command, CommandName::StartExam, START);
                assert_eq!(
                    stored_command::Entity::find()
                        .count(&f.service.admin().0.writer)
                        .await
                        .unwrap(),
                    0
                );
                f.report(
                    command_id,
                    CommandName::StartExam,
                    CommandStatus::Completed,
                    None,
                )
                .await
                .unwrap();
                assert!(f.service.due_commands().await.unwrap().is_empty());
            }
            END => {
                assert_eq!(commands.len(), 1);
                command(&commands[0].command, CommandName::EndExam, END);
            }
            _ => assert!(commands.is_empty()),
        }
    }
}

#[tokio::test]
async fn out_of_order_events_preserve_ack_and_reject_conflicting_recording_summaries() {
    let f = Fixture::memory().await;
    let hello = ready();
    f.connect(hello.clone()).await;
    let (third, summary) = recording_event(3, 'c');
    f.receive(third).await;
    sync(&f.connect(hello.clone()).await, ExamPhase::Scheduled, 0);
    let (first, _) = recording_event(1, 'b');
    assert!(matches!(
        f.service.receive(f.exam, USER, first).await,
        Err(DomainError::InvalidInput)
    ));
    let (first, _) = recording_event(1, 'c');
    f.receive(first).await;
    sync(&f.connect(hello.clone()).await, ExamPhase::Scheduled, 1);
    let occurred_at = time(START - 8);
    f.receive(incident_event(2, occurred_at)).await;
    sync(&f.connect(hello).await, ExamPhase::Scheduled, 3);
    let offset = UtcOffset::from_hms(8, 0, 0).unwrap();
    f.receive(incident_event(2, occurred_at.to_offset(offset)))
        .await;
    assert!(matches!(
        f.service
            .receive(
                f.exam,
                USER,
                incident_event(2, occurred_at + Duration::NANOSECOND)
            )
            .await,
        Err(DomainError::InvalidInput)
    ));
    assert!(matches!(
        f.service
            .receive(f.exam, USER, incident_event(0, occurred_at))
            .await,
        Err(DomainError::InvalidInput)
    ));
    let AdminOutput::Incidents(incidents) = f
        .admin(AdminOperation::IncidentsList {
            exam_id: f.exam,
            oj_user_id: None,
            kind: Some("navigation_denied".into()),
            since: Some(occurred_at),
            until: Some(occurred_at.to_offset(offset)),
        })
        .await
    else {
        panic!("expected incidents")
    };
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].occurred_at, occurred_at);
    let recordings = AdminOperation::RecordingsShow {
        exam_id: f.exam,
        oj_user_id: USER,
    };
    let admin = f.service.admin();
    let query = || admin.execute("tester".into(), recordings.clone());
    assert!(matches!(query().await, Err(DomainError::NotFound)));
    f.receive(ClientMessage::Status {
        state: state(ExecutionState::Finalizing, RecordingState::Failed),
    })
    .await;
    assert!(matches!(query().await, Err(DomainError::NotFound)));
    f.receive(ClientMessage::Status {
        state: state(ExecutionState::Finalizing, RecordingState::Complete),
    })
    .await;
    let AdminOutput::Recording(recording) = f
        .admin(AdminOperation::RecordingsShow {
            exam_id: f.exam,
            oj_user_id: USER,
        })
        .await
    else {
        panic!("expected recording")
    };
    assert_eq!(recording, summary);

    f.receive(incident_event(i64::MAX, occurred_at)).await;
    let AdminOutput::Events(events) = f
        .admin(AdminOperation::EventsList {
            exam_id: f.exam,
            oj_user_id: USER,
        })
        .await
    else {
        panic!("expected events")
    };
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [1, 2, 3, i64::MAX]
    );
}

#[tokio::test]
async fn replacement_issues_a_fresh_command_and_records_incidents() {
    let f = Fixture::memory().await;
    f.set(START);
    let old = f.connect(ready()).await;
    let old_id = command(&old.commands[0], CommandName::StartExam, START);
    f.receive(ClientMessage::Status {
        state: state(ExecutionState::Recording, RecordingState::Capturing),
    })
    .await;
    let new = f.connect(ready()).await;
    let new_id = command(&new.commands[0], CommandName::StartExam, START);
    assert_ne!(new_id, old_id);
    let incidents = f.incidents().await;
    assert!(incidents.windows(2).all(|pair| pair[0].id < pair[1].id));
    let mut kinds = incidents
        .iter()
        .map(|item| item.kind.as_str())
        .collect::<Vec<_>>();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            "connection_lost",
            "connection_restored",
            "process_restarted"
        ]
    );
    assert!(incidents.iter().all(|item| item.occurred_at == time(START)));
    assert_eq!(
        incidents
            .iter()
            .find(|incident| incident.kind == "connection_lost")
            .unwrap()
            .detail
            .as_deref(),
        Some("replaced")
    );
}

fn valid_states() -> [ClientState; 8] {
    [
        state(ExecutionState::Ready, RecordingState::Idle),
        state(ExecutionState::Recording, RecordingState::Capturing),
        state(ExecutionState::Recording, RecordingState::Failed),
        state(ExecutionState::Finalizing, RecordingState::Finalizing),
        state(ExecutionState::Finalizing, RecordingState::Complete),
        state(ExecutionState::Finalizing, RecordingState::Failed),
        state(ExecutionState::Finished, RecordingState::Complete),
        state(ExecutionState::Finished, RecordingState::Failed),
    ]
}

async fn persisted_state(f: &Fixture) -> ClientState {
    let key = db::StudentKey::new(f.exam, USER).unwrap();
    db::Runtime::get(&f.service.admin().0.writer, &key)
        .await
        .unwrap()
        .state()
        .unwrap()
}

#[tokio::test]
async fn status_updates_follow_execution_transition_matrix() {
    for from in valid_states()
        .into_iter()
        .filter(|state| state.execution != ExecutionState::Finished)
    {
        for to in valid_states() {
            let f = Fixture::memory().await;
            f.set(START);
            f.connect(hello(from.execution, from.recording)).await;
            let result = f
                .service
                .receive(f.exam, USER, ClientMessage::Status { state: to })
                .await;
            let regression = matches!(
                (from.execution, to.execution),
                (ExecutionState::Recording, ExecutionState::Ready)
                    | (
                        ExecutionState::Finalizing,
                        ExecutionState::Ready | ExecutionState::Recording
                    )
            );
            let expected = if regression {
                assert!(
                    matches!(result, Err(DomainError::InvalidInput)),
                    "{from:?} -> {to:?}: {result:?}"
                );
                from
            } else {
                result.unwrap();
                if to.execution == ExecutionState::Finished {
                    from
                } else {
                    to
                }
            };
            assert_eq!(persisted_state(&f).await, expected, "{from:?} -> {to:?}");
            assert!(f.runtime().await.connected);
            assert!(f.incidents().await.is_empty());
        }
    }
}

#[tokio::test]
async fn status_updates_cannot_replace_a_committed_terminal_state() {
    for recording in [RecordingState::Complete, RecordingState::Failed] {
        let f = Fixture::memory().await;
        f.set(END);
        let outcome = f
            .connect(hello(ExecutionState::Finalizing, recording))
            .await;
        let end = command(&outcome.commands[0], CommandName::EndExam, END);
        let (status, error) = if recording == RecordingState::Complete {
            let (event, _) = recording_event(1, 'b');
            f.receive(event).await;
            (CommandStatus::Completed, None)
        } else {
            (
                CommandStatus::Failed,
                Some(error_code::RECORDING_FINISH_FAILED),
            )
        };
        f.report(end, CommandName::EndExam, status, error)
            .await
            .unwrap();
        let committed = state(ExecutionState::Finished, recording);
        for reported in valid_states() {
            f.receive(ClientMessage::Status { state: reported }).await;
            assert_eq!(persisted_state(&f).await, committed);
        }
        f.connect(hello(ExecutionState::Finished, recording)).await;
    }
}

#[tokio::test]
async fn rejects_a_recording_summary_with_the_same_hash_and_different_count() {
    let f = Fixture::memory().await;
    f.connect(ready()).await;
    let (event, summary) = recording_event(1, 'c');
    f.receive(event).await;
    let conflicting = RecordingSummary::new(2, summary.merged_sha256).unwrap();
    for seq in [1, 2] {
        let event = ClientMessage::Event {
            seq,
            event: ExamEvent::RecordingFinalized(conflicting.clone()),
        };
        assert!(matches!(
            f.service.receive(f.exam, USER, event).await,
            Err(DomainError::InvalidInput)
        ));
    }
}

#[tokio::test]
async fn end_success_requires_the_contiguous_recording_summary() {
    let f = Fixture::memory().await;
    f.set(END);
    let outcome = f
        .connect(hello(ExecutionState::Finalizing, RecordingState::Complete))
        .await;
    let end = command(&outcome.commands[0], CommandName::EndExam, END);
    let finished = |recording| {
        f.service
            .connect(f.exam, USER, hello(ExecutionState::Finished, recording))
    };
    assert!(matches!(
        finished(RecordingState::Complete).await,
        Err(DomainError::Conflict)
    ));
    // The previous server could persist a finished hello without an end result.
    let database = f.service.admin().0.writer;
    crate::entity::runtime::Entity::update(crate::entity::runtime::ActiveModel {
        exam_id: Set(f.exam.to_string()),
        oj_user_id: Set(USER),
        execution: Set("finished".into()),
        ..Default::default()
    })
    .exec_without_returning(&database)
    .await
    .unwrap();
    assert!(matches!(
        finished(RecordingState::Complete).await,
        Err(DomainError::Conflict)
    ));
    f.connect(hello(ExecutionState::Finalizing, RecordingState::Complete))
        .await;
    let report = || f.report(end, CommandName::EndExam, CommandStatus::Completed, None);
    assert!(matches!(report().await, Err(DomainError::Conflict)));
    let (finalized, _) = recording_event(2, 'b');
    f.receive(finalized).await;
    assert!(matches!(report().await, Err(DomainError::Conflict)));
    assert_eq!(f.runtime().await.execution, ExecutionState::Finalizing);
    assert!(
        stored_command::Entity::find_by_id(end.to_string())
            .one(&f.service.admin().0.writer)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        f.report(
            end,
            CommandName::EndExam,
            CommandStatus::Failed,
            Some(error_code::RECORDING_FINISH_FAILED)
        )
        .await,
        Err(DomainError::Conflict)
    ));
    f.receive(incident_event(1, time(END))).await;
    report().await.unwrap();
    assert_eq!(f.runtime().await.execution, ExecutionState::Finished);
    let replay = f
        .connect(hello(ExecutionState::Finalizing, RecordingState::Failed))
        .await;
    let replay = command(&replay.commands[0], CommandName::EndExam, END);
    assert!(matches!(
        f.report(
            replay,
            CommandName::EndExam,
            CommandStatus::Failed,
            Some(error_code::RECORDING_FINISH_FAILED)
        )
        .await,
        Err(DomainError::Conflict)
    ));
    assert!(matches!(
        finished(RecordingState::Failed).await,
        Err(DomainError::Conflict)
    ));
    f.service
        .disconnect(f.exam, USER, DisconnectReason::Closed)
        .await
        .unwrap();
    assert!(!f.runtime().await.connected);
    assert_eq!(f.incidents().await.len(), 1);
    sync(
        &finished(RecordingState::Complete).await.unwrap(),
        ExamPhase::Ended,
        2,
    );
}

#[tokio::test]
async fn command_failure_codes_are_specific_to_the_command() {
    let f = Fixture::memory().await;
    f.set(START);
    let start = command(
        &f.connect(ready()).await.commands[0],
        CommandName::StartExam,
        START,
    );
    assert!(matches!(
        f.report(
            start,
            CommandName::StartExam,
            CommandStatus::Failed,
            Some(error_code::RECORDING_FINISH_FAILED)
        )
        .await,
        Err(DomainError::Conflict)
    ));
    f.report(
        start,
        CommandName::StartExam,
        CommandStatus::Failed,
        Some(error_code::RECORDING_START_FAILED),
    )
    .await
    .unwrap();
    f.set(END);
    let end = command(
        &f.service.due_commands().await.unwrap()[0].command,
        CommandName::EndExam,
        END,
    );
    let report = |code| f.report(end, CommandName::EndExam, CommandStatus::Failed, Some(code));
    assert!(matches!(
        report(error_code::RECORDING_START_FAILED).await,
        Err(DomainError::Conflict)
    ));
    f.receive(incident_event(1, time(END))).await;
    report(error_code::RECORDING_FINISH_FAILED).await.unwrap();
    assert_eq!(f.runtime().await.execution, ExecutionState::Finished);
}

#[tokio::test]
async fn server_startup_cleans_stale_connections_but_admin_open_leaves_them_online() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("exam.sqlite").display()
    );
    let f = Fixture::open(&url).await;
    f.connect(ready()).await;
    let admin = AdminService::open(&url).await.unwrap();
    let query = AdminOperation::StudentsShow {
        exam_id: f.exam,
        oj_user_id: USER,
    };
    let AdminOutput::Student(runtime) =
        admin.execute("tester".into(), query.clone()).await.unwrap()
    else {
        panic!("expected runtime")
    };
    assert!(runtime.connected);
    let reopened = ExamService::open_with_clock(&url, || time(START))
        .await
        .unwrap();
    reopened.recover_online().await.unwrap();
    reopened.recover_online().await.unwrap();
    let AdminOutput::Student(runtime) = admin.execute("tester".into(), query).await.unwrap() else {
        panic!("expected runtime")
    };
    assert!(!runtime.connected);
    let incidents = f.incidents().await;
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].kind, "connection_lost");
    assert_eq!(incidents[0].detail.as_deref(), Some("server_restart"));
    assert_eq!(incidents[0].occurred_at, time(START));
}

#[tokio::test]
async fn failed_audit_rolls_back_enrollment() {
    let f = Fixture::memory().await;
    f.trigger_ddl(
        "CREATE TRIGGER fail_audit BEFORE INSERT ON admin_audit
           BEGIN SELECT RAISE(ABORT, 'test storage failure'); END",
    )
    .await;
    let op = AdminOperation::ParticipantsAdd {
        exam_id: f.exam,
        oj_user_id: USER + 1,
    };
    assert!(matches!(
        f.service.admin().execute("tester".into(), op.clone()).await,
        Err(DomainError::Storage { .. })
    ));
    assert!(matches!(
        f.service.check_enrollment(f.exam, USER + 1).await,
        Err(DomainError::NotEnrolled)
    ));
    f.trigger_ddl("DROP TRIGGER fail_audit").await;
    f.admin(op).await;
    f.service.check_enrollment(f.exam, USER + 1).await.unwrap();
    let audits = audit::Entity::find()
        .all(&f.service.admin().0.writer)
        .await
        .unwrap();
    let operations = audits
        .iter()
        .map(|row| db::decode::<serde_json::Value>(&row.operation).unwrap())
        .collect::<Vec<_>>();
    assert!(operations.contains(&serde_json::json!({
        "operation": "participants_add",
        "exam_id": f.exam,
        "oj_user_id": USER + 1,
    })));
    assert!(
        operations
            .iter()
            .any(|operation| operation["operation"] == "exams_create"
                && operation["exam_id"] == f.exam.to_string())
    );
}

#[tokio::test]
async fn failed_recovery_incident_rolls_back_connection_replacement() {
    let f = Fixture::memory().await;
    f.set(START);
    let outcome = f.connect(ready()).await;
    let command_id = command(&outcome.commands[0], CommandName::StartExam, START);
    f.trigger_ddl(
        "CREATE TRIGGER fail_restore BEFORE INSERT ON incidents
           WHEN NEW.kind = 'connection_restored'
           BEGIN SELECT RAISE(ABORT, 'test storage failure'); END",
    )
    .await;
    let replacement = hello(ExecutionState::Recording, RecordingState::Capturing);
    assert!(matches!(
        f.service.connect(f.exam, USER, replacement.clone()).await,
        Err(DomainError::Storage { .. })
    ));
    let runtime = f.runtime().await;
    assert!(runtime.connected);
    assert_eq!(runtime.execution, ExecutionState::Ready);
    assert!(f.incidents().await.is_empty());
    f.report(
        command_id,
        CommandName::StartExam,
        CommandStatus::Completed,
        None,
    )
    .await
    .unwrap();

    f.trigger_ddl("DROP TRIGGER fail_restore").await;
    f.connect(replacement).await;
    assert_eq!(f.incidents().await.len(), 3);
}

#[tokio::test]
async fn failed_disconnect_keeps_the_runtime_online_and_rolls_back_incident() {
    let f = Fixture::memory().await;
    f.connect(ready()).await;
    f.trigger_ddl(
        "CREATE TRIGGER fail_offline BEFORE UPDATE OF connected ON student_runtimes
           BEGIN SELECT RAISE(ABORT, 'test storage failure'); END",
    )
    .await;
    let observed_at = time(START);
    let disconnect = || {
        f.service
            .disconnect_at(f.exam, USER, DisconnectReason::Closed, observed_at)
    };
    assert!(matches!(
        disconnect().await,
        Err(DomainError::Storage { .. })
    ));
    assert!(f.runtime().await.connected);
    assert!(f.incidents().await.is_empty());

    f.set(END);
    f.trigger_ddl("DROP TRIGGER fail_offline").await;
    disconnect().await.unwrap();
    disconnect().await.unwrap();
    assert!(!f.runtime().await.connected);
    let incidents = f.incidents().await;
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].kind, "connection_lost");
    assert_eq!(incidents[0].occurred_at, observed_at);
}

#[tokio::test]
async fn a_locked_disconnect_can_be_retried_at_the_original_time() {
    use sea_orm::TransactionTrait;

    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("exam.sqlite").display()
    );
    let f = Fixture::open(&url).await;
    f.connect(ready()).await;

    let writer = sea_orm::Database::connect(&url).await.unwrap();
    let transaction = writer.begin().await.unwrap();
    stored_exam::Entity::update(stored_exam::ActiveModel {
        exam_id: Set(f.exam.to_string()),
        title: Set("Holding the write lock".into()),
        ..Default::default()
    })
    .exec_without_returning(&transaction)
    .await
    .unwrap();

    let disconnect = || {
        f.service.disconnect_at(
            f.exam,
            USER,
            DisconnectReason::HeartbeatTimeout,
            time(START),
        )
    };
    assert!(disconnect().await.unwrap_err().is_storage_busy());
    transaction.rollback().await.unwrap();
    assert!(f.runtime().await.connected);
    assert!(f.incidents().await.is_empty());

    f.set(END);
    disconnect().await.unwrap();
    disconnect().await.unwrap();
    assert!(!f.runtime().await.connected);
    let incidents = f.incidents().await;
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].kind, "connection_lost");
    assert_eq!(incidents[0].occurred_at, time(START));
}

#[tokio::test]
async fn failed_ack_update_rolls_back_event_and_derived_incident() {
    let f = Fixture::memory().await;
    let hello = ready();
    f.connect(hello.clone()).await;
    f.trigger_ddl(
        "CREATE TRIGGER fail_ack BEFORE UPDATE OF ack_event_through ON student_runtimes
           BEGIN SELECT RAISE(ABORT, 'test storage failure'); END",
    )
    .await;
    let event = incident_event(1, time(START));
    assert!(matches!(
        f.service.receive(f.exam, USER, event.clone()).await,
        Err(DomainError::Storage { .. })
    ));
    assert!(f.incidents().await.is_empty());
    let AdminOutput::Events(events) = f
        .admin(AdminOperation::EventsList {
            exam_id: f.exam,
            oj_user_id: USER,
        })
        .await
    else {
        panic!("expected events")
    };
    assert!(events.is_empty());
    f.trigger_ddl("DROP TRIGGER fail_ack").await;
    sync(&f.connect(hello.clone()).await, ExamPhase::Scheduled, 0);
    f.receive(event.clone()).await;
    f.receive(event).await;
    sync(&f.connect(hello).await, ExamPhase::Scheduled, 1);
    assert_eq!(
        f.incidents()
            .await
            .iter()
            .filter(|entry| entry.kind == "navigation_denied")
            .count(),
        1
    );
}

#[tokio::test]
async fn failed_runtime_update_rolls_back_end_command_result() {
    let f = Fixture::memory().await;
    f.set(END);
    let outcome = f
        .connect(hello(ExecutionState::Finalizing, RecordingState::Complete))
        .await;
    let command_id = command(&outcome.commands[0], CommandName::EndExam, END);
    f.receive(recording_event(1, 'b').0).await;
    f.trigger_ddl(
        "CREATE TRIGGER fail_result BEFORE UPDATE OF execution ON student_runtimes
           BEGIN SELECT RAISE(ABORT, 'test storage failure'); END",
    )
    .await;
    assert!(matches!(
        f.report(
            command_id,
            CommandName::EndExam,
            CommandStatus::Completed,
            None
        )
        .await,
        Err(DomainError::Storage { .. })
    ));
    assert_eq!(f.runtime().await.execution, ExecutionState::Finalizing);
    f.trigger_ddl("DROP TRIGGER fail_result").await;
    f.report(
        command_id,
        CommandName::EndExam,
        CommandStatus::Failed,
        Some(error_code::INTERNAL_ERROR),
    )
    .await
    .unwrap();
    assert_eq!(f.runtime().await.execution, ExecutionState::Finished);
    let row = stored_command::Entity::find_by_id(command_id.to_string())
        .one(&f.service.admin().0.writer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        db::decode::<serde_json::Value>(&row.result).unwrap(),
        serde_json::json!({"status": "failed", "error_code": error_code::INTERNAL_ERROR})
    );
}

#[tokio::test]
async fn migrations_create_the_final_tables() {
    use migration::{Migrator, MigratorTrait};
    use sea_orm::Statement;

    let fixture = Fixture::memory().await;
    let database = fixture.service.admin().0.writer;
    assert_eq!(
        Migrator::get_applied_migrations(&database)
            .await
            .unwrap()
            .len(),
        3
    );
    let tables = database
        .query_all_raw(Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name",
        ))
        .await
        .unwrap();
    let names = tables
        .iter()
        .map(|row| row.try_get_by_index::<String>(0).unwrap());
    assert!(names.eq([
        "admin_audit",
        "commands",
        "enrollments",
        "events",
        "exams",
        "incidents",
        "seaql_migrations",
        "student_enrollments",
        "student_runtimes",
    ]));
}

#[tokio::test]
async fn student_enrollment_migration_preserves_numeric_enrollments_and_runtime() {
    use migration::{Migrator, MigratorTrait};

    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("old.sqlite").display()
    );
    let fixture = Fixture::open(&url).await;
    let exam_id = fixture.exam;
    let admin = fixture.service.admin();
    Migrator::down(&admin.0.writer, Some(1)).await.unwrap();
    fixture.connect(ready()).await;
    fixture.service.close().await.unwrap();

    let upgraded = ExamService::open_with_clock(&url, || time(START - 1))
        .await
        .unwrap();
    assert_eq!(upgraded.list_for(USER).await.unwrap()[0].id, exam_id);
    assert!(matches!(
        upgraded
            .admin()
            .execute(
                "tester".into(),
                AdminOperation::StudentsShow {
                    exam_id,
                    oj_user_id: USER,
                }
            )
            .await,
        Ok(AdminOutput::Student(_))
    ));
    assert!(matches!(
        upgraded.admin().execute("tester".into(), AdminOperation::StudentEnrollmentsList {
            exam_id,
        }).await,
        Ok(AdminOutput::StudentEnrollments(rows)) if rows.is_empty()
    ));
}

#[tokio::test]
async fn initial_migration_rolls_back_database_with_evidence() {
    use migration::{Migrator, MigratorTrait};

    let f = Fixture::memory().await;
    f.connect(ready()).await;
    let (event, _) = recording_event(1, 'b');
    f.receive(event).await;
    f.receive(incident_event(2, time(START))).await;
    f.service
        .disconnect(f.exam, USER, DisconnectReason::Closed)
        .await
        .unwrap();
    let service = f.service.admin();
    Migrator::down(&service.0.writer, None).await.unwrap();
    Migrator::up(&service.0.writer, None).await.unwrap();
    assert!(matches!(
        f.service.check_enrollment(f.exam, USER).await,
        Err(DomainError::NotFound)
    ));
}

#[tokio::test]
async fn stale_write_snapshot_rechecks_enrollment_after_concurrent_cli_commit() {
    use sea_orm::{ConnectOptions, Database, sqlx::sqlite::SqliteJournalMode};

    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("exam.sqlite").display()
    );
    // WAL persists in the file after closing this setup connection.
    let mut options = ConnectOptions::new(&url);
    options.map_sqlx_sqlite_opts(|opts| opts.journal_mode(SqliteJournalMode::Wal));
    Database::connect(options)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    let f = Fixture::open(&url).await;
    let admin = ExamService::open_with_clock(&url, || time(START - 1))
        .await
        .unwrap()
        .admin();
    let database = f.service.admin().0.writer;
    let key = db::StudentKey::new(f.exam, USER).unwrap();
    let mut attempts = 0;
    let outcome = db::write(&database, |tx| {
        attempts += 1;
        let key = key.clone();
        let concurrent_admin = (attempts == 1).then(|| admin.clone());
        Box::pin(async move {
            db::enrollment(tx, &key).await?;
            if let Some(admin) = concurrent_admin {
                admin
                    .execute(
                        "other".into(),
                        AdminOperation::ParticipantsRemove {
                            exam_id: key.exam_id(),
                            oj_user_id: key.user,
                        },
                    )
                    .await
                    .unwrap();
            }
            stored_exam::Entity::update(stored_exam::ActiveModel {
                exam_id: Set(key.exam),
                title: Set("stale write".into()),
                ..Default::default()
            })
            .exec_without_returning(tx)
            .await?;
            Ok(())
        })
    })
    .await;
    assert!(matches!(outcome, Err(DomainError::NotEnrolled)));
    assert_eq!(attempts, 2);
    let exam = f.admin(AdminOperation::ExamsShow { exam_id: f.exam }).await;
    let AdminOutput::ExamDetail { summary, .. } = exam else {
        panic!("expected exam")
    };
    assert_eq!(summary.title, "Programming");
}

#[tokio::test]
async fn wal_and_read_only_pool_configured_for_file_databases() {
    use sea_orm::{DbBackend, Statement};

    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("exam.sqlite").display()
    );
    let f = Fixture::open(&url).await;
    let service = f.service.admin().0;

    let journal: String = service
        .writer
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA journal_mode",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_eq!(journal.to_ascii_lowercase(), "wal");

    let synchronous: i64 = service
        .writer
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA synchronous",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_eq!(synchronous, 2);

    let foreign_keys: i64 = service
        .reader
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA foreign_keys",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_eq!(foreign_keys, 1);

    let write_attempt = service
        .reader
        .execute_unprepared("UPDATE exams SET title = 'blocked'")
        .await;
    assert!(write_attempt.is_err());

    f.connect(ready()).await;
    let candidates = f.service.due_command_candidates().await.unwrap();
    assert!(candidates.is_empty());
}

#[tokio::test]
async fn due_schedule_query_uses_indexes_and_filters_unenrolled_or_offline() {
    use sea_orm::{DbBackend, Statement};

    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("exam.sqlite").display()
    );
    let f = Fixture::open(&url).await;
    f.admin(AdminOperation::ParticipantsAdd {
        exam_id: f.exam,
        oj_user_id: USER + 1,
    })
    .await;
    f.admin(AdminOperation::ParticipantsAdd {
        exam_id: f.exam,
        oj_user_id: USER + 2,
    })
    .await;
    f.connect(ready()).await;
    f.service.connect(f.exam, USER + 1, ready()).await.unwrap();
    f.service.connect(f.exam, USER + 2, ready()).await.unwrap();
    f.service
        .disconnect(f.exam, USER + 2, DisconnectReason::Closed)
        .await
        .unwrap();
    f.admin(AdminOperation::ParticipantsRemove {
        exam_id: f.exam,
        oj_user_id: USER + 1,
    })
    .await;

    let query = crate::exam::build_due_schedule_query();
    let (sql, values) = query.build(sea_orm::sea_query::SqliteQueryBuilder);
    let explain = Statement::from_sql_and_values(
        DbBackend::Sqlite,
        format!("EXPLAIN QUERY PLAN {sql}"),
        values,
    );
    let rows = f
        .service
        .admin()
        .0
        .reader
        .query_all_raw(explain)
        .await
        .unwrap();
    let plan: Vec<String> = rows
        .into_iter()
        .map(|row| row.try_get_by_index::<String>(3).unwrap())
        .collect();
    let joined_plan = plan.join(" | ");
    assert!(
        joined_plan.contains("USING INDEX student_runtimes_online"),
        "expected partial index usage on student_runtimes, got: {joined_plan}"
    );
    assert!(
        joined_plan.contains("sqlite_autoindex_enrollments_1"),
        "expected PK index usage on enrollments, got: {joined_plan}"
    );

    f.set(START);
    let candidates = f.service.due_command_candidates().await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].oj_user_id, USER);
    assert!(candidates[0].connection_id.is_some());
}

#[tokio::test]
async fn execute_batch_isolates_business_errors_with_savepoints() {
    use crate::{BatchOutput, BatchWrite};

    let f = Fixture::memory().await;
    f.admin(AdminOperation::ParticipantsAdd {
        exam_id: f.exam,
        oj_user_id: USER + 1,
    })
    .await;
    let conn1 = Uuid::new_v4();
    let conn2 = Uuid::new_v4();
    let ClientMessage::Event { event: nav, .. } = incident_event(1, time(START)) else {
        unreachable!()
    };

    let outcomes = f
        .service
        .execute_batch(
            vec![
                BatchWrite::Connect {
                    exam_id: f.exam,
                    oj_user_id: USER,
                    connection_id: conn1,
                    hello: ready(),
                    observed_at: time(START),
                },
                BatchWrite::Connect {
                    exam_id: f.exam,
                    oj_user_id: USER + 1,
                    connection_id: conn2,
                    hello: ready(),
                    observed_at: time(START),
                },
                BatchWrite::Event {
                    exam_id: f.exam,
                    oj_user_id: USER,
                    connection_id: Some(conn1),
                    seq: 1,
                    event: nav.clone(),
                    observed_at: time(START),
                },
                BatchWrite::Status {
                    exam_id: f.exam,
                    oj_user_id: USER,
                    connection_id: Some(conn1),
                    state: state(ExecutionState::Recording, RecordingState::Capturing),
                },
                // Invalid transition (Recording -> Ready) must roll back only its savepoint.
                BatchWrite::Status {
                    exam_id: f.exam,
                    oj_user_id: USER,
                    connection_id: Some(conn1),
                    state: state(ExecutionState::Ready, RecordingState::Idle),
                },
                // Subsequent valid write for USER + 1 must still commit.
                BatchWrite::Event {
                    exam_id: f.exam,
                    oj_user_id: USER + 1,
                    connection_id: Some(conn2),
                    seq: 1,
                    event: nav,
                    observed_at: time(START),
                },
            ],
            Vec::new(),
        )
        .await
        .unwrap();

    assert!(matches!(outcomes[0], Ok(BatchOutput::Connected(_))));
    assert!(matches!(outcomes[1], Ok(BatchOutput::Connected(_))));
    assert!(matches!(outcomes[2], Ok(BatchOutput::Ack)));
    assert!(matches!(outcomes[3], Ok(BatchOutput::Ack)));
    assert!(matches!(outcomes[4], Err(DomainError::InvalidInput)));
    assert!(matches!(outcomes[5], Ok(BatchOutput::Ack)));
    assert_eq!(f.runtime().await.execution, ExecutionState::Recording);
}

#[tokio::test]
async fn execute_batch_rolls_back_entire_batch_on_storage_error() {
    use crate::BatchWrite;

    let f = Fixture::memory().await;
    f.admin(AdminOperation::ParticipantsAdd {
        exam_id: f.exam,
        oj_user_id: USER + 1,
    })
    .await;
    let conn1 = Uuid::new_v4();
    let conn2 = Uuid::new_v4();
    f.service
        .execute_batch(
            vec![
                BatchWrite::Connect {
                    exam_id: f.exam,
                    oj_user_id: USER,
                    connection_id: conn1,
                    hello: ready(),
                    observed_at: time(START),
                },
                BatchWrite::Connect {
                    exam_id: f.exam,
                    oj_user_id: USER + 1,
                    connection_id: conn2,
                    hello: ready(),
                    observed_at: time(START),
                },
            ],
            Vec::new(),
        )
        .await
        .unwrap();
    let ClientMessage::Event { event: nav, .. } = incident_event(1, time(START)) else {
        unreachable!()
    };
    f.trigger_ddl(
        "CREATE TRIGGER fail_second_event BEFORE INSERT ON events
           WHEN NEW.oj_user_id = 8
           BEGIN SELECT RAISE(ABORT, 'batch storage fault'); END",
    )
    .await;
    let err = f
        .service
        .execute_batch(
            vec![
                BatchWrite::Event {
                    exam_id: f.exam,
                    oj_user_id: USER,
                    connection_id: Some(conn1),
                    seq: 1,
                    event: nav.clone(),
                    observed_at: time(START + 1),
                },
                BatchWrite::Event {
                    exam_id: f.exam,
                    oj_user_id: USER + 1,
                    connection_id: Some(conn2),
                    seq: 1,
                    event: nav,
                    observed_at: time(START + 1),
                },
            ],
            Vec::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Storage { .. }));
    let AdminOutput::Events(user_events) = f
        .admin(AdminOperation::EventsList {
            exam_id: f.exam,
            oj_user_id: USER,
        })
        .await
    else {
        panic!("expected events")
    };
    assert!(user_events.is_empty());
}

#[tokio::test]
async fn execute_batch_flushes_heartbeats_for_matching_active_connections_only() {
    use crate::{BatchWrite, HeartbeatUpdate};

    let f = Fixture::memory().await;
    f.admin(AdminOperation::ParticipantsAdd {
        exam_id: f.exam,
        oj_user_id: USER + 1,
    })
    .await;
    f.admin(AdminOperation::ParticipantsAdd {
        exam_id: f.exam,
        oj_user_id: USER + 2,
    })
    .await;
    let conn1 = Uuid::new_v4();
    let conn2 = Uuid::new_v4();
    let conn3 = Uuid::new_v4();
    f.service
        .execute_batch(
            vec![
                BatchWrite::Connect {
                    exam_id: f.exam,
                    oj_user_id: USER,
                    connection_id: conn1,
                    hello: ready(),
                    observed_at: time(START - 10),
                },
                BatchWrite::Connect {
                    exam_id: f.exam,
                    oj_user_id: USER + 1,
                    connection_id: conn2,
                    hello: ready(),
                    observed_at: time(START - 10),
                },
                BatchWrite::Connect {
                    exam_id: f.exam,
                    oj_user_id: USER + 2,
                    connection_id: conn3,
                    hello: ready(),
                    observed_at: time(START - 10),
                },
            ],
            Vec::new(),
        )
        .await
        .unwrap();
    f.service
        .disconnect(f.exam, USER + 2, DisconnectReason::Closed)
        .await
        .unwrap();

    // Flush heartbeats with: valid conn1, stale generation for USER + 1, offline USER + 2.
    f.service
        .execute_batch(
            Vec::new(),
            vec![
                HeartbeatUpdate {
                    exam_id: f.exam,
                    oj_user_id: USER,
                    connection_id: conn1,
                    last_seen_at: time(START + 5),
                },
                HeartbeatUpdate {
                    exam_id: f.exam,
                    oj_user_id: USER + 1,
                    connection_id: Uuid::new_v4(),
                    last_seen_at: time(START + 5),
                },
                HeartbeatUpdate {
                    exam_id: f.exam,
                    oj_user_id: USER + 2,
                    connection_id: conn3,
                    last_seen_at: time(START + 5),
                },
            ],
        )
        .await
        .unwrap();

    assert_eq!(f.runtime().await.last_seen_at, Some(time(START + 5)));
    let AdminOutput::Student(other) = f
        .admin(AdminOperation::StudentsShow {
            exam_id: f.exam,
            oj_user_id: USER + 1,
        })
        .await
    else {
        panic!("expected student")
    };
    assert_eq!(other.last_seen_at, Some(time(START - 10)));
    let AdminOutput::Student(offline) = f
        .admin(AdminOperation::StudentsShow {
            exam_id: f.exam,
            oj_user_id: USER + 2,
        })
        .await
    else {
        panic!("expected student")
    };
    assert_eq!(offline.last_seen_at, Some(time(START - 10)));
}

#[tokio::test]
async fn immediate_transaction_acquires_write_lock_upfront() {
    use sea_orm::ConnectionTrait;

    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("exam.sqlite").display()
    );
    let f = Fixture::open(&url).await;
    let writer_pool = f.service.admin().0.writer;

    let mut opts = sea_orm::ConnectOptions::new(&url);
    opts.map_sqlx_sqlite_opts(|o| o.busy_timeout(std::time::Duration::from_millis(10)));
    let second = sea_orm::Database::connect(opts).await.unwrap();

    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let holder = tokio::spawn(async move {
        db::immediate_transaction(&writer_pool, |_tx| {
            Box::pin(async move {
                let _ = locked_tx.send(());
                let _ = release_rx.await;
                Ok(())
            })
        })
        .await
    });
    locked_rx.await.unwrap();

    // Another writer trying to acquire BEGIN IMMEDIATE must fail with SQLITE_BUSY immediately
    // even though the holder has not executed any SQL write statement inside its transaction.
    let err = db::immediate_transaction(&second, |tx| {
        Box::pin(async move {
            tx.execute_unprepared("SELECT 1").await?;
            Ok(())
        })
    })
    .await
    .unwrap_err();
    assert!(
        err.is_storage_busy(),
        "expected BEGIN IMMEDIATE lock contention, got: {err:?}"
    );

    let _ = release_tx.send(());
    holder.await.unwrap().unwrap();

    // Verify `second` pool remains healthy after a busy BEGIN IMMEDIATE failure.
    db::immediate_transaction(&second, |tx| {
        Box::pin(async move {
            tx.execute_unprepared("SELECT 1").await?;
            Ok(())
        })
    })
    .await
    .unwrap();
}
