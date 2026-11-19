use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

use cvbbmso_protocol::{
    ClientHello, ClientIncident, ClientMessage, ClientState, CommandName, CommandStatus, ExamEvent,
    ExamPhase, ExecutionState, IncidentCode, Platform, RecordingManifest, RecordingPolicy,
    RecordingState, SegmentMeta, ServerMessage, Sha256Digest, error_code,
};
use sea_orm::ConnectionTrait;
use time::{Duration, OffsetDateTime, UtcOffset};
use uuid::Uuid;

use crate::{
    AdminOperation, AdminOutput, AdminService, ConnectOutcome, DisconnectReason, DomainError,
    ExamService, IncidentRecord, StudentRuntime, db,
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

    async fn receive(&self, message: ClientMessage) -> Vec<ServerMessage> {
        self.service
            .receive(self.exam, USER, message)
            .await
            .unwrap()
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

    async fn sql(&self, sql: &str) {
        self.service
            .admin()
            .0
            .database
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

fn sync(outcome: &ConnectOutcome, expected_phase: ExamPhase, expected_ack: u64) {
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

fn completed(command_id: Uuid) -> ClientMessage {
    ClientMessage::CommandResult {
        command_id,
        status: CommandStatus::Completed,
        error_code: None,
    }
}

fn failed(command_id: Uuid) -> ClientMessage {
    ClientMessage::CommandResult {
        command_id,
        status: CommandStatus::Failed,
        error_code: Some(error_code::INTERNAL_ERROR.to_owned()),
    }
}

fn incident_event(seq: u64, timestamp: OffsetDateTime) -> ClientMessage {
    ClientMessage::Event {
        seq,
        event: ExamEvent::Incident(ClientIncident {
            code: IncidentCode::NavigationDenied,
            occurred_at: timestamp,
            detail: None,
        }),
    }
}

fn recording_event(seq: u64, hash: char) -> (ClientMessage, RecordingManifest) {
    let segment = SegmentMeta::new(
        1,
        time(START),
        time(START + 1),
        Sha256Digest::parse(&"a".repeat(64)).unwrap(),
    )
    .unwrap();
    let manifest = RecordingManifest::new(
        vec![segment],
        Sha256Digest::parse(&hash.to_string().repeat(64)).unwrap(),
    )
    .unwrap();
    (
        ClientMessage::Event {
            seq,
            event: ExamEvent::RecordingFinalized(manifest.clone()),
        },
        manifest,
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
    assert!(f.service.list_for(USER + 1).await.unwrap().is_empty());
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
    assert!(f.runtime().await.connected);
    f.receive(incident_event(2, time(START - 1))).await;
    assert_eq!(f.incidents().await.len(), 2);
    f.admin(AdminOperation::ParticipantsAdd {
        exam_id: f.exam,
        oj_user_id: USER,
    })
    .await;
    sync(&f.connect(hello).await, ExamPhase::Scheduled, 2);
}

#[tokio::test]
async fn time_boundaries_schedule_each_pending_command_once() {
    let f = Fixture::memory().await;
    let hello = ready();
    let outcome = f.connect(hello).await;
    sync(&outcome, ExamPhase::Scheduled, 0);
    assert!(outcome.commands.is_empty());

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
                assert!(f.service.due_commands().await.unwrap().is_empty());
                assert_eq!(
                    f.receive(completed(command_id)).await,
                    [ServerMessage::CommandAck { command_id }]
                );
            }
            END => {
                assert_eq!(commands.len(), 1);
                command(&commands[0].command, CommandName::EndExam, END);
                assert!(f.service.due_commands().await.unwrap().is_empty());
            }
            _ => assert!(commands.is_empty()),
        }
    }
}

#[tokio::test]
async fn out_of_order_events_preserve_ack_evidence_and_latest_manifest() {
    let f = Fixture::memory().await;
    let hello = ready();
    f.connect(hello.clone()).await;
    let (third, latest) = recording_event(3, 'c');
    f.receive(third).await;
    sync(&f.connect(hello.clone()).await, ExamPhase::Scheduled, 0);
    let (first, _) = recording_event(1, 'b');
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
    let AdminOutput::Recording(recording) = f
        .admin(AdminOperation::RecordingsShow {
            exam_id: f.exam,
            oj_user_id: USER,
        })
        .await
    else {
        panic!("expected recording")
    };
    assert_eq!(recording, latest);

    f.receive(incident_event(u64::MAX, occurred_at)).await;
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
        [1, 2, 3, u64::MAX]
    );
}

#[tokio::test]
async fn replacement_retires_unreported_commands_and_keeps_incident_order() {
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
    assert!(matches!(
        f.service.receive(f.exam, USER, completed(old_id)).await,
        Err(DomainError::Conflict)
    ));
    let incidents = f.incidents().await;
    assert_eq!(
        incidents
            .iter()
            .map(|item| item.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "connection_lost",
            "connection_restored",
            "process_restarted"
        ]
    );
    assert!(incidents.iter().all(|item| item.occurred_at == time(START)));
    assert_eq!(incidents[0].detail.as_deref(), Some("replaced"));
}

#[tokio::test]
async fn command_replays_and_lost_end_ack_are_idempotent() {
    let f = Fixture::memory().await;
    f.set(START);
    let outcome = f.connect(ready()).await;
    let start = command(&outcome.commands[0], CommandName::StartExam, START);
    let start_ack = ServerMessage::CommandAck { command_id: start };
    assert_eq!(
        f.receive(completed(start)).await,
        std::slice::from_ref(&start_ack)
    );
    assert_eq!(f.receive(completed(start)).await, [start_ack]);
    assert!(matches!(
        f.service.receive(f.exam, USER + 1, completed(start)).await,
        Err(DomainError::Conflict)
    ));
    assert!(matches!(
        f.service.receive(f.exam, USER, failed(start)).await,
        Err(DomainError::Conflict)
    ));
    assert!(matches!(
        f.service
            .receive(f.exam, USER, completed(Uuid::nil()))
            .await,
        Err(DomainError::Conflict)
    ));

    f.set(END);
    let due = f.service.due_commands().await.unwrap();
    let end = command(&due[0].command, CommandName::EndExam, END);
    let (finalized, _) = recording_event(1, 'b');
    f.receive(finalized.clone()).await;
    let end_ack = ServerMessage::CommandAck { command_id: end };
    assert_eq!(
        f.receive(completed(end)).await,
        std::slice::from_ref(&end_ack)
    );
    f.service
        .disconnect(f.exam, USER, DisconnectReason::Closed)
        .await
        .unwrap();
    assert!(!f.runtime().await.connected);
    assert!(f.incidents().await.is_empty());

    let outcome = f
        .connect(hello(ExecutionState::Finalizing, RecordingState::Complete))
        .await;
    sync(&outcome, ExamPhase::Ended, 1);
    let reopened = command(&outcome.commands[0], CommandName::EndExam, END);
    assert_ne!(reopened, end);
    assert_eq!(f.receive(completed(end)).await, [end_ack]);
    assert_eq!(f.runtime().await.execution, ExecutionState::Finalizing);
    f.receive(finalized).await;
    assert_eq!(
        f.receive(completed(reopened)).await,
        [ServerMessage::CommandAck {
            command_id: reopened
        }]
    );
    f.service
        .disconnect(f.exam, USER, DisconnectReason::Closed)
        .await
        .unwrap();
    assert_eq!(f.runtime().await.execution, ExecutionState::Finished);
    assert!(f.incidents().await.is_empty());
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
    assert_eq!(incidents[0].detail.as_deref(), Some("closed"));
    assert_eq!(incidents[0].occurred_at, time(START));
}

#[tokio::test]
async fn failed_audit_rolls_back_enrollment() {
    let f = Fixture::memory().await;
    f.sql(
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
    f.sql("DROP TRIGGER fail_audit").await;
    f.admin(op).await;
    f.service.check_enrollment(f.exam, USER + 1).await.unwrap();
    let row = f.service.admin().0.database.query_one(db::sql!(
        "SELECT COUNT(*) AS count FROM admin_audit WHERE operation = 'participants_add' AND target_users = ?",
        format!("[{}]", USER + 1),
    )).await.unwrap().unwrap();
    assert_eq!(row.try_get::<i64>("", "count").unwrap(), 1);
}

#[tokio::test]
async fn failed_recovery_incident_rolls_back_connection_replacement() {
    let f = Fixture::memory().await;
    f.set(START);
    let outcome = f.connect(ready()).await;
    let command_id = command(&outcome.commands[0], CommandName::StartExam, START);
    f.sql(
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
    assert_eq!(
        f.receive(completed(command_id)).await,
        [ServerMessage::CommandAck { command_id }]
    );

    f.sql("DROP TRIGGER fail_restore").await;
    f.connect(replacement).await;
    assert_eq!(f.incidents().await.len(), 3);
}

#[tokio::test]
async fn failed_ack_update_rolls_back_event_and_derived_incident() {
    let f = Fixture::memory().await;
    let hello = ready();
    f.connect(hello.clone()).await;
    f.sql(
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
    f.sql("DROP TRIGGER fail_ack").await;
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
async fn failed_runtime_update_rolls_back_command_result_and_ack() {
    let f = Fixture::memory().await;
    f.set(START);
    let outcome = f.connect(ready()).await;
    let command_id = command(&outcome.commands[0], CommandName::StartExam, START);
    f.sql(
        "CREATE TRIGGER fail_result BEFORE UPDATE OF execution ON student_runtimes
           BEGIN SELECT RAISE(ABORT, 'test storage failure'); END",
    )
    .await;
    assert!(matches!(
        f.service.receive(f.exam, USER, completed(command_id)).await,
        Err(DomainError::Storage { .. })
    ));
    assert_eq!(f.runtime().await.execution, ExecutionState::Ready);
    f.sql("DROP TRIGGER fail_result").await;
    assert_eq!(
        f.receive(failed(command_id)).await,
        [ServerMessage::CommandAck { command_id }]
    );
    assert_eq!(f.runtime().await.execution, ExecutionState::Recording);
    let row = f
        .service
        .admin()
        .0
        .database
        .query_one(db::sql!(
            "SELECT result_error_code FROM commands WHERE command_id = ?",
            command_id.to_string(),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.try_get::<String>("", "result_error_code").unwrap(),
        error_code::INTERNAL_ERROR
    );
}

#[tokio::test]
async fn initial_migration_rolls_back_database_with_evidence() {
    use sea_orm_migration::MigratorTrait;

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
    crate::migration::Migrator::down(&service.0.database, None)
        .await
        .unwrap();
    crate::migration::Migrator::up(&service.0.database, None)
        .await
        .unwrap();
    assert!(matches!(
        f.service.check_enrollment(f.exam, USER).await,
        Err(DomainError::NotFound)
    ));
}

#[tokio::test]
async fn stale_write_snapshot_rechecks_enrollment_after_concurrent_cli_commit() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("exam.sqlite").display()
    );
    let f = Fixture::open(&url).await;
    f.sql("PRAGMA journal_mode = WAL").await;
    let admin = AdminService::open(&url).await.unwrap();
    let database = f.service.admin().0.database;
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
            tx.execute(db::sql!(
                "UPDATE exams SET title = 'stale write' WHERE exam_id = ?",
                key.exam,
            ))
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
