use std::{
    io,
    sync::atomic::{AtomicI64, Ordering},
};

use axum::{
    body::{Body, to_bytes},
    http::{Request, Response},
    serve::Listener,
};
use cvbbmso_domain::{AdminOperation, AdminOutput, AdminService};
use cvbbmso_protocol::{
    ClientHello, ClientMessage, ClientState, CommandName, CommandStatus, ExamEvent, ExamPhase,
    ExecutionState, Platform, RecordingManifest, RecordingPolicy, RecordingState, SegmentMeta,
    ServerMessage, Sha256Digest,
};
use futures_util::{SinkExt, StreamExt};
use time::OffsetDateTime;
use tokio::{
    io::{DuplexStream, duplex},
    sync::mpsc,
    task::JoinHandle,
};
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{Error as ClientError, Message, client::IntoClientRequest},
};
use tower::ServiceExt;

use super::*;
use crate::hub::Manager;

const STUDENT: i64 = 17;
const OTHER_STUDENT: i64 = 29;
const COOKIE: &str = "site=\"%2B=\"; connect.sid=s%3Aexample==; aux=value";
const START: i64 = 2_000_000_000;
const END: i64 = START + 60;
const TIMEOUT: Duration = Duration::from_secs(25);
type TestSocket = WebSocketStream<DuplexStream>;

struct MemoryListener(mpsc::UnboundedReceiver<DuplexStream>);

impl Listener for MemoryListener {
    type Io = DuplexStream;
    type Addr = ();

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.0.recv().await {
            Some(stream) => (stream, ()),
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(())
    }
}

struct Fixture {
    state: AppState,
    exam_id: Uuid,
    admin: AdminService,
    clock: Arc<AtomicI64>,
    database_url: String,
    _files: tempfile::TempDir,
    tasks: Vec<JoinHandle<()>>,
    clients: mpsc::UnboundedSender<DuplexStream>,
}

impl Fixture {
    async fn new() -> Self {
        let clock = Arc::new(AtomicI64::new(START - 1));
        let injected_clock = Arc::clone(&clock);
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("exam.sqlite");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let exam = Arc::new(
            ExamService::open_with_clock(&url, move || {
                instant(injected_clock.load(Ordering::Relaxed))
            })
            .await
            .unwrap(),
        );
        let admin = exam.admin();
        let output = admin
            .execute(
                "staff".into(),
                AdminOperation::ExamsCreate {
                    title: "Final exam".into(),
                    start_at: instant(START),
                    end_at: instant(END),
                    policy: RecordingPolicy::new(30, 15).unwrap(),
                },
            )
            .await
            .unwrap();
        let AdminOutput::ExamCreated { exam_id } = output else {
            panic!("expected created exam");
        };
        admin
            .execute(
                "staff".into(),
                AdminOperation::ParticipantsAdd {
                    exam_id,
                    oj_user_id: STUDENT,
                },
            )
            .await
            .unwrap();
        let (hub, manager) = Manager::new(Arc::clone(&exam), TIMEOUT);
        let state = AppState {
            exam,
            verifier: Arc::new(IdentityVerifier::Test(Box::new(|cookies| {
                let header = cookies.header_value();
                assert!(header.is_sensitive());
                let id = match header.to_str().unwrap() {
                    COOKIE => STUDENT,
                    "connect.sid=other" => OTHER_STUDENT,
                    "connect.sid=expired" => return Err(AccodingError::Unauthenticated),
                    "connect.sid=upstream" => return Err(AccodingError::InvalidResponse(None)),
                    _ => panic!("unexpected synthetic cookie"),
                };
                Ok(AccodingUser::new(id, format!("student{id}"), "Student".into()).unwrap())
            }))),
            hub,
            timeout: TIMEOUT,
        };
        let (clients, connections) = mpsc::unbounded_channel();
        let app = router(state.clone());
        let tasks = vec![
            tokio::spawn(async move { manager.run().await.unwrap() }),
            tokio::spawn(
                async move { axum::serve(MemoryListener(connections), app).await.unwrap() },
            ),
        ];
        Self {
            state,
            exam_id,
            admin,
            clock,
            database_url: url,
            _files: files,
            tasks,
            clients,
        }
    }

    fn path(&self) -> String {
        format!("/api/exams/{}/ws", self.exam_id)
    }

    async fn upgrade(&self, path: &str, cookie: &str) -> Result<TestSocket, ClientError> {
        let (client, server) = duplex(64 * 1024);
        self.clients.send(server).unwrap();
        let mut request = format!("ws://localhost{path}")
            .into_client_request()
            .unwrap();
        for value in cookie.split('\n') {
            request
                .headers_mut()
                .append(header::COOKIE, value.parse().unwrap());
        }
        let (socket, response) = client_async(request, client).await?;
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        Ok(socket)
    }

    async fn request(&self, request: Request<Body>) -> Response<Body> {
        router(self.state.clone()).oneshot(request).await.unwrap()
    }

    async fn admin(&self, operation: AdminOperation) -> AdminOutput {
        self.admin.execute("staff".into(), operation).await.unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn instant(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

fn hello(execution: ExecutionState, recording: RecordingState) -> ClientMessage {
    ClientMessage::Hello(ClientHello {
        device_id: Uuid::new_v4(),
        instance_id: Uuid::new_v4(),
        platform: Platform::Windows,
        state: ClientState {
            execution,
            recording,
        },
    })
}

async fn send(socket: &mut TestSocket, message: &ClientMessage) {
    socket
        .send(Message::Text(
            serde_json::to_string(message).unwrap().into(),
        ))
        .await
        .unwrap();
}

async fn message(socket: &mut TestSocket) -> ServerMessage {
    let frame = socket.next().await.unwrap().unwrap();
    let Message::Text(text) = frame else {
        panic!("expected message, got {frame:?}");
    };
    serde_json::from_str(&text).unwrap()
}

async fn socket_error(socket: &mut TestSocket, expected: &str) {
    assert!(matches!(message(socket).await, ServerMessage::Error { code, .. } if code == expected));
    assert!(matches!(
        socket.next().await.unwrap().unwrap(),
        Message::Close(_)
    ));
}

async fn body(response: Response<Body>) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn protected_http_routes_use_verified_identity_and_json_errors() {
    let fixture = Fixture::new().await;
    let health = fixture
        .request(Request::get("/api/health").body(Body::empty()).unwrap())
        .await;
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(body(health).await, json!({ "status": "ok" }));

    let exams = fixture
        .request(
            Request::get("/api/exams")
                .header(header::COOKIE, "site=\"%2B=\"")
                .header(header::COOKIE, "connect.sid=s%3Aexample==;\taux=value")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(exams.status(), StatusCode::OK);
    let exams: ExamListResponse = serde_json::from_value(body(exams).await).unwrap();
    assert_eq!(exams.user.id, STUDENT);
    assert_eq!(exams.items.len(), 1);
    assert_eq!(exams.items[0].id, fixture.exam_id);
    assert_eq!(exams.items[0].phase, ExamPhase::Scheduled);

    let exams = fixture
        .request(
            Request::get("/api/exams")
                .header(header::COOKIE, "connect.sid=other")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let exams: ExamListResponse = serde_json::from_value(body(exams).await).unwrap();
    assert_eq!(exams.user.id, OTHER_STUDENT);
    assert_eq!(exams.items, []);

    for (cookie, status, code) in [
        (
            None,
            StatusCode::UNAUTHORIZED,
            error_code::ACCODING_AUTH_EXPIRED,
        ),
        (
            Some("connect.sid="),
            StatusCode::UNAUTHORIZED,
            error_code::ACCODING_AUTH_EXPIRED,
        ),
        (
            Some("connect.sid=expired"),
            StatusCode::UNAUTHORIZED,
            error_code::ACCODING_AUTH_EXPIRED,
        ),
        (
            Some("connect.sid=upstream"),
            StatusCode::SERVICE_UNAVAILABLE,
            error_code::ACCODING_UNAVAILABLE,
        ),
        (
            Some("connect.sid=x; a"),
            StatusCode::BAD_REQUEST,
            error_code::INVALID_REQUEST,
        ),
        (
            Some("connect.sid=x; connect.sid=y"),
            StatusCode::BAD_REQUEST,
            error_code::INVALID_REQUEST,
        ),
    ] {
        let mut request = Request::get("/api/exams");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let response = fixture.request(request.body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), status);
        assert_eq!(body(response).await["error"]["code"], code);
    }

    for request in [
        Request::get(fixture.path()),
        Request::post("/api/exams"),
        Request::get("/unknown"),
    ] {
        let response = fixture.request(request.body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body(response).await["error"]["code"],
            error_code::INVALID_REQUEST
        );
    }
}

#[tokio::test]
async fn upgrades_require_a_valid_exam_and_enrollment() {
    let fixture = Fixture::new().await;
    for (path, cookie, status, code) in [
        (
            "/api/exams/invalid/ws".into(),
            COOKIE,
            StatusCode::BAD_REQUEST,
            error_code::INVALID_REQUEST,
        ),
        (
            fixture.path(),
            "connect.sid=other",
            StatusCode::FORBIDDEN,
            error_code::NOT_ENROLLED,
        ),
        (
            format!("/api/exams/{}/ws", Uuid::new_v4()),
            COOKIE,
            StatusCode::NOT_FOUND,
            error_code::EXAM_NOT_FOUND,
        ),
    ] {
        let Err(ClientError::Http(response)) = fixture.upgrade(&path, cookie).await else {
            panic!("expected an HTTP error from WebSocket upgrade");
        };
        assert_eq!(response.status(), status);
        let body: serde_json::Value =
            serde_json::from_slice(response.body().as_ref().unwrap()).unwrap();
        assert_eq!(body["error"]["code"], code);
    }
}

#[tokio::test]
async fn validates_first_message_and_rechecks_enrollment_after_upgrade() {
    let fixture = Fixture::new().await;
    let path = fixture.path();
    for frame in [
        Message::Text(r#"{"type":"heartbeat"}"#.into()),
        Message::Text("{".into()),
        Message::Binary(vec![0].into()),
    ] {
        let mut socket = fixture.upgrade(&path, COOKIE).await.unwrap();
        socket.send(frame).await.unwrap();
        socket_error(&mut socket, error_code::PROTOCOL_ERROR).await;
    }

    let mut socket = fixture.upgrade(&path, COOKIE).await.unwrap();
    fixture
        .admin(AdminOperation::ParticipantsRemove {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await;
    send(
        &mut socket,
        &hello(ExecutionState::Ready, RecordingState::Idle),
    )
    .await;
    socket_error(&mut socket, error_code::NOT_ENROLLED).await;
    let AdminOutput::Students(students) = fixture
        .admin(AdminOperation::StudentsStatus {
            exam_id: fixture.exam_id,
        })
        .await
    else {
        panic!("expected student list");
    };
    assert!(students.is_empty());
}

async fn confirm(socket: &mut TestSocket, command_id: Uuid) {
    send(
        socket,
        &ClientMessage::CommandResult {
            command_id,
            status: CommandStatus::Completed,
            error_code: None,
        },
    )
    .await;
    assert_eq!(
        message(socket).await,
        ServerMessage::CommandAck { command_id }
    );
}

async fn command(socket: &mut TestSocket, expected: CommandName, effective_at: i64) -> Uuid {
    let ServerMessage::Command {
        command_id,
        name,
        effective_at: actual,
    } = message(socket).await
    else {
        panic!("expected command");
    };
    assert_eq!(name, expected);
    assert_eq!(actual, instant(effective_at));
    command_id
}

#[tokio::test]
async fn starts_schedules_and_finishes_an_exam_over_websocket() {
    let fixture = Fixture::new().await;
    let mut socket = fixture.upgrade(&fixture.path(), COOKIE).await.unwrap();
    send(
        &mut socket,
        &hello(ExecutionState::Ready, RecordingState::Idle),
    )
    .await;
    assert_eq!(
        message(&mut socket).await,
        ServerMessage::Sync {
            server_time: instant(START - 1),
            phase: ExamPhase::Scheduled,
            start_at: instant(START),
            end_at: instant(END),
            recording_policy: RecordingPolicy::new(30, 15).unwrap(),
            ack_event_through: 0,
        }
    );

    fixture.clock.store(START, Ordering::Relaxed);
    fixture.state.hub.trigger_schedule().await;
    let start_command = command(&mut socket, CommandName::StartExam, START).await;
    confirm(&mut socket, start_command).await;
    send(
        &mut socket,
        &ClientMessage::Status {
            state: ClientState {
                execution: ExecutionState::Recording,
                recording: RecordingState::Capturing,
            },
        },
    )
    .await;
    send(&mut socket, &ClientMessage::Heartbeat).await;
    let digest = Sha256Digest::parse(&"a".repeat(64)).unwrap();
    let segment = SegmentMeta::new(1, instant(START), instant(START + 30), digest.clone()).unwrap();
    send(
        &mut socket,
        &ClientMessage::Event {
            seq: 1,
            event: ExamEvent::SegmentFinalized(segment.clone()),
        },
    )
    .await;

    fixture.clock.store(END, Ordering::Relaxed);
    fixture.state.hub.trigger_schedule().await;
    let end_command = command(&mut socket, CommandName::EndExam, END).await;
    assert_ne!(start_command, end_command);
    let manifest = RecordingManifest::new(vec![segment], digest).unwrap();
    send(
        &mut socket,
        &ClientMessage::Event {
            seq: 2,
            event: ExamEvent::RecordingFinalized(manifest.clone()),
        },
    )
    .await;
    confirm(&mut socket, end_command).await;
    socket.send(Message::Close(None)).await.unwrap();
    let _ = socket.next().await;
    fixture.state.hub.barrier().await;

    assert_finished_recording(&fixture, &manifest).await;
}

async fn assert_finished_recording(fixture: &Fixture, manifest: &RecordingManifest) {
    let AdminOutput::Student(runtime) = fixture
        .admin(AdminOperation::StudentsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected student");
    };
    assert_eq!(runtime.execution, ExecutionState::Finished);
    assert!(!runtime.connected);
    let AdminOutput::Events(events) = fixture
        .admin(AdminOperation::EventsList {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected event list");
    };
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let AdminOutput::Recording(recording) = fixture
        .admin(AdminOperation::RecordingsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected recording");
    };
    assert_eq!(&recording, manifest);
    let AdminOutput::Incidents(incidents) = fixture
        .admin(AdminOperation::IncidentsList {
            exam_id: fixture.exam_id,
            oj_user_id: Some(STUDENT),
            kind: None,
            since: None,
            until: None,
        })
        .await
    else {
        panic!("expected incident list");
    };
    assert!(incidents.is_empty());
}

async fn events(fixture: &Fixture) -> Vec<cvbbmso_domain::EventRecord> {
    let AdminOutput::Events(events) = fixture
        .admin(AdminOperation::EventsList {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected events");
    };
    events
}

async fn incidents(fixture: &Fixture) -> Vec<cvbbmso_domain::IncidentRecord> {
    let AdminOutput::Incidents(incidents) = fixture
        .admin(AdminOperation::IncidentsList {
            exam_id: fixture.exam_id,
            oj_user_id: Some(STUDENT),
            kind: None,
            since: None,
            until: None,
        })
        .await
    else {
        panic!("expected incidents");
    };
    incidents
}

fn navigation(seq: u64, detail: &str) -> ClientMessage {
    ClientMessage::Event {
        seq,
        event: ExamEvent::Incident(cvbbmso_protocol::ClientIncident {
            code: cvbbmso_protocol::IncidentCode::NavigationDenied,
            occurred_at: instant(START),
            detail: Some(detail.into()),
        }),
    }
}

async fn open_socket(
    fixture: &Fixture,
    state: &ClientMessage,
    ack_event_through: u64,
) -> TestSocket {
    let mut socket = fixture.upgrade(&fixture.path(), COOKIE).await.unwrap();
    send(&mut socket, state).await;
    let ServerMessage::Sync {
        ack_event_through: actual,
        ..
    } = message(&mut socket).await
    else {
        panic!("expected sync first");
    };
    assert_eq!(actual, ack_event_through);
    socket
}

async fn close_socket(fixture: &Fixture, socket: &mut TestSocket) {
    socket.send(Message::Close(None)).await.unwrap();
    let _ = socket.next().await;
    fixture.state.hub.barrier().await;
}

#[tokio::test]
async fn resumes_event_sequence_across_connections() {
    let fixture = Fixture::new().await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut socket = open_socket(&fixture, &state, 0).await;
    for (seq, after) in [(3, 0), (1, 1), (2, 3)] {
        send(&mut socket, &navigation(seq, "blocked")).await;
        close_socket(&fixture, &mut socket).await;
        socket = open_socket(&fixture, &state, after).await;
    }
    send(&mut socket, &navigation(2, "blocked")).await;
    send(&mut socket, &navigation(2, "conflicts")).await;
    socket_error(&mut socket, error_code::PROTOCOL_ERROR).await;
    assert_eq!(
        events(&fixture)
            .await
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        incidents(&fixture)
            .await
            .iter()
            .filter(|incident| incident.kind == "navigation_denied")
            .count(),
        3
    );
}

#[tokio::test]
async fn replays_end_handshake_after_ack_loss() {
    let fixture = Fixture::new().await;
    let state = hello(ExecutionState::Finalizing, RecordingState::Complete);
    fixture.clock.store(END, Ordering::Relaxed);
    let mut socket = open_socket(&fixture, &state, 0).await;
    let previous = command(&mut socket, CommandName::EndExam, END).await;
    let digest = Sha256Digest::parse(&"b".repeat(64)).unwrap();
    let segment = SegmentMeta::new(1, instant(START), instant(END), digest.clone()).unwrap();
    let manifest = RecordingManifest::new(vec![segment], digest).unwrap();
    let finalized = ClientMessage::Event {
        seq: 1,
        event: ExamEvent::RecordingFinalized(manifest),
    };
    send(&mut socket, &finalized).await;
    let result = ClientMessage::CommandResult {
        command_id: previous,
        status: CommandStatus::Completed,
        error_code: None,
    };
    send(&mut socket, &result).await;
    // The close is handled after the queued result. The old ACK is deliberately unread.
    socket.send(Message::Close(None)).await.unwrap();
    while let Some(Ok(frame)) = socket.next().await {
        if matches!(frame, Message::Close(_)) {
            break;
        }
    }
    fixture.state.hub.barrier().await;
    assert!(incidents(&fixture).await.is_empty());

    let mut resumed = open_socket(&fixture, &state, 1).await;
    let current = command(&mut resumed, CommandName::EndExam, END).await;
    assert_ne!(previous, current);
    send(&mut resumed, &finalized).await;
    send(&mut resumed, &result).await;
    assert_eq!(
        message(&mut resumed).await,
        ServerMessage::CommandAck {
            command_id: previous
        }
    );
    confirm(&mut resumed, current).await;
    close_socket(&fixture, &mut resumed).await;
    assert!(incidents(&fixture).await.is_empty());
    assert_eq!(events(&fixture).await.len(), 1);
}

#[tokio::test]
async fn replacement_ignores_old_close_timeout_and_event_notices() {
    use crate::hub::{Connection, Peer};
    use cvbbmso_domain::DisconnectReason;
    use tokio::{sync::oneshot, time::Instant};

    let fixture = Fixture::new().await;
    let student = StudentKey {
        exam_id: fixture.exam_id,
        oj_user_id: STUDENT,
    };
    let previous = Connection {
        student,
        id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
    };
    let ClientMessage::Hello(previous_hello) =
        hello(ExecutionState::Recording, RecordingState::Capturing)
    else {
        unreachable!()
    };
    let (frames, mut outgoing) = mpsc::channel(2);
    let (close, closing) = oneshot::channel();
    let (admitted, admission) = oneshot::channel();
    assert!(
        fixture
            .state
            .hub
            .connect(
                previous,
                previous_hello,
                Peer {
                    frames,
                    close,
                    admitted
                }
            )
            .await
    );
    admission.await.unwrap();
    assert!(matches!(
        outgoing.recv().await,
        Some(ServerMessage::Sync { .. })
    ));

    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut current = open_socket(&fixture, &state, 0).await;
    assert!(closing.await.unwrap().is_none());
    fixture
        .state
        .hub
        .end(previous, DisconnectReason::Closed, None)
        .await;
    fixture
        .state
        .hub
        .end(previous, DisconnectReason::HeartbeatTimeout, None)
        .await;
    assert!(
        fixture
            .state
            .hub
            .receive(previous, navigation(1, "stale"), Instant::now())
            .await
    );
    fixture.state.hub.barrier().await;
    assert!(events(&fixture).await.is_empty());
    let incidents = incidents(&fixture).await;
    assert_eq!(
        incidents
            .iter()
            .map(|incident| incident.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "connection_lost",
            "connection_restored",
            "process_restarted"
        ]
    );
    assert_eq!(incidents[0].detail.as_deref(), Some("replaced"));
    send(&mut current, &navigation(1, "current")).await;
    close_socket(&fixture, &mut current).await;
    assert_eq!(events(&fixture).await.len(), 1);
}

#[tokio::test]
async fn heartbeat_renews_the_exact_deadline_and_timeout_is_recorded_once() {
    use crate::hub::{Connection, Peer};
    use cvbbmso_domain::DisconnectReason;
    use tokio::{
        sync::oneshot,
        time::{self, Instant},
    };

    let fixture = Fixture::new().await;
    let student = StudentKey {
        exam_id: fixture.exam_id,
        oj_user_id: STUDENT,
    };
    let connection = Connection {
        student,
        id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
    };
    let ClientMessage::Hello(hello) = hello(ExecutionState::Ready, RecordingState::Idle) else {
        unreachable!()
    };
    let (frames, mut outgoing) = mpsc::channel(2);
    let (close, mut closing) = oneshot::channel();
    let (admitted, admission) = oneshot::channel();
    time::pause();
    // SQLite performs work on another thread. Keep Tokio from advancing time
    // automatically while that work is in flight.
    let keep_clock_paused = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    assert!(
        fixture
            .state
            .hub
            .connect(
                connection,
                hello,
                Peer {
                    frames,
                    close,
                    admitted
                }
            )
            .await
    );
    let original_deadline = admission.await.unwrap();
    assert!(matches!(
        outgoing.recv().await,
        Some(ServerMessage::Sync { .. })
    ));

    time::advance(TIMEOUT.checked_sub(Duration::from_secs(1)).unwrap()).await;
    let received_at = Instant::now();
    assert!(
        fixture
            .state
            .hub
            .receive(connection, ClientMessage::Heartbeat, received_at)
            .await
    );
    fixture.state.hub.barrier().await;
    time::advance(original_deadline - Instant::now()).await;
    fixture
        .state
        .hub
        .end(connection, DisconnectReason::HeartbeatTimeout, None)
        .await;
    fixture.state.hub.barrier().await;
    assert!(matches!(
        closing.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(incidents(&fixture).await.is_empty());

    time::advance(received_at + TIMEOUT - Instant::now()).await;
    fixture
        .state
        .hub
        .end(connection, DisconnectReason::HeartbeatTimeout, None)
        .await;
    assert!(closing.await.unwrap().is_none());
    fixture
        .state
        .hub
        .end(connection, DisconnectReason::HeartbeatTimeout, None)
        .await;
    fixture.state.hub.barrier().await;
    let incidents = incidents(&fixture).await;
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].kind, "connection_lost");
    assert_eq!(incidents[0].detail.as_deref(), Some("heartbeat_timeout"));
    keep_clock_paused.abort();
}

#[tokio::test]
async fn recovers_online_state_after_restart_and_reissues_pending_command() {
    let fixture = Fixture::new().await;
    fixture.clock.store(START, Ordering::Relaxed);
    let hello = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut old_socket = open_socket(&fixture, &hello, 0).await;
    let old_id = command(&mut old_socket, CommandName::StartExam, START).await;

    for task in &fixture.tasks {
        task.abort();
    }
    let exam = ExamService::open_with_clock(&fixture.database_url, || instant(START))
        .await
        .unwrap();
    exam.recover_online().await.unwrap();
    let AdminOutput::Student(runtime) = fixture
        .admin(AdminOperation::StudentsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected student");
    };
    assert!(!runtime.connected);
    let incidents_before_connect = incidents(&fixture).await;
    assert_eq!(incidents_before_connect.len(), 1);
    assert_eq!(incidents_before_connect[0].kind, "connection_lost");
    let ClientMessage::Hello(hello) = hello else {
        unreachable!();
    };
    let outcome = exam.connect(fixture.exam_id, STUDENT, hello).await.unwrap();
    assert!(matches!(
        outcome.sync,
        ServerMessage::Sync {
            phase: ExamPhase::Running,
            ack_event_through: 0,
            ..
        }
    ));
    assert!(matches!(
        outcome.commands.as_slice(),
        [ServerMessage::Command { name: CommandName::StartExam, command_id, .. }] if *command_id != old_id
    ));
}

#[tokio::test]
async fn undeliverable_command_disconnects_and_reconnect_gets_a_new_command() {
    use crate::hub::{Connection, Peer};
    use tokio::sync::oneshot;

    let fixture = Fixture::new().await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let ClientMessage::Hello(hello) = state.clone() else {
        unreachable!();
    };
    let student = StudentKey {
        exam_id: fixture.exam_id,
        oj_user_id: STUDENT,
    };
    let connection = Connection {
        student,
        id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
    };
    let (frames, mut outgoing) = mpsc::channel(2);
    let (close, closing) = oneshot::channel();
    let (admitted, admission) = oneshot::channel();
    assert!(
        fixture
            .state
            .hub
            .connect(
                connection,
                hello,
                Peer {
                    frames,
                    close,
                    admitted
                }
            )
            .await
    );
    admission.await.unwrap();
    assert!(matches!(
        outgoing.recv().await,
        Some(ServerMessage::Sync { .. })
    ));
    drop(outgoing);

    fixture.clock.store(START, Ordering::Relaxed);
    fixture.state.hub.trigger_schedule().await;
    assert!(closing.await.unwrap().is_none());
    let incidents = incidents(&fixture).await;
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].detail.as_deref(), Some("closed"));

    let mut socket = open_socket(&fixture, &state, 0).await;
    let command_id = command(&mut socket, CommandName::StartExam, START).await;
    confirm(&mut socket, command_id).await;
}

#[tokio::test]
async fn request_logs_include_metadata_and_keep_cookie_values_private() {
    use std::sync::Mutex;

    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let fixture = Fixture::new().await;
    let output = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::clone(&output);
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || Capture(Arc::clone(&writer)))
        .finish();
    // Keep callsite interest stable while other tests use the same Router.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let response = fixture
        .request(
            Request::get("/api/exams")
                .header(header::COOKIE, COOKIE)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let output = output.lock().unwrap();
    let output = std::str::from_utf8(&output).unwrap();
    assert!(!output.contains("connect.sid"));
    assert!(!output.contains("s%3Aexample"));
    assert!(output.lines().any(|line| {
        let log: serde_json::Value = serde_json::from_str(line).unwrap();
        log["fields"]["status"] == 200
            && log["span"]["route"] == "/api/exams"
            && log["span"]["oj_user_id"] == STUDENT
            && log["span"]["request_id"].as_str().is_some()
    }));
}
