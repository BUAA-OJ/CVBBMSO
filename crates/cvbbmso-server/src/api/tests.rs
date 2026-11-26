use std::{
    cell::RefCell,
    io,
    net::Ipv4Addr,
    sync::{
        Once,
        atomic::{AtomicI64, Ordering},
    },
};

use axum::{
    body::{Body, to_bytes},
    http::{Request, Response},
    serve::Listener,
};
use cvbbmso_domain::{AdminOperation, AdminOutput, AdminService};
use cvbbmso_protocol::{
    ClientHello, ClientMessage, ClientState, CommandName, CommandStatus, ExamEvent, ExamPhase,
    ExecutionState, Platform, RecordingPolicy, RecordingState, RecordingSummary, SegmentMeta,
    ServerMessage, Sha256Digest,
};
use futures_util::{SinkExt, StreamExt};
use time::OffsetDateTime;
use tokio::{
    io::{DuplexStream, duplex},
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
};
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{
        Error as ClientError, Message,
        client::IntoClientRequest,
        protocol::frame::{
            Frame,
            coding::{Data, OpCode},
        },
    },
};
use tower::ServiceExt;

use super::*;
use crate::{ServerConfig, ServerError, hub::Manager, serve};

mod extremes;
mod peer;

use peer::{TestPeer, result};

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
    write_retries: Arc<tokio::sync::Notify>,
    batch_waiting: Arc<tokio::sync::Notify>,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_exam(END, 30).await
    }

    async fn with_exam(end: i64, segment_seconds: u32) -> Self {
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
                    end_at: instant(end),
                    policy: RecordingPolicy::new(segment_seconds, 15).unwrap(),
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
        let write_retries = Arc::clone(&manager.write_retries);
        let batch_waiting = Arc::clone(&manager.batch_waiting);
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
                    "connect.sid=upstream-json" => {
                        let error =
                            serde_json::from_str::<i64>(r#""private test upstream response""#)
                                .unwrap_err();
                        return Err(AccodingError::InvalidResponse(Some(error)));
                    }
                    _ => panic!("unexpected synthetic cookie"),
                };
                Ok(AccodingUser::new(id, format!("student{id}"), "Student".into()).unwrap())
            }))),
            auth_semaphore: Arc::new(tokio::sync::Semaphore::new(64)),
            auth_wait_timeout: Duration::from_secs(5),
            hub,
            timeout: TIMEOUT,
        };
        let (clients, connections) = mpsc::unbounded_channel();
        let app = router(state.clone());
        let tasks = vec![
            tokio::spawn(manager.run()),
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
            write_retries,
            batch_waiting,
        }
    }

    fn path(&self) -> String {
        format!("/api/exams/{}/ws", self.exam_id)
    }

    async fn upgrade(&self, path: &str, cookie: &str) -> Result<TestSocket, ClientError> {
        // Fit an oversized test frame and its header in one write so the
        // server can reject it without interrupting the test client's send.
        let (client, server) = duplex(128 * 1024);
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

    async fn direct_admin(&self, operation: AdminOperation) -> AdminOutput {
        self.admin.execute("staff".into(), operation).await.unwrap()
    }

    async fn admin(&self, operation: AdminOperation) -> AdminOutput {
        self.state
            .hub
            .admin("staff".into(), operation)
            .await
            .unwrap()
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

thread_local! {
    static LOG_OUTPUT: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

struct LogWriter;

impl io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        LOG_OUTPUT.with_borrow_mut(|output| {
            if let Some(output) = output {
                output.extend_from_slice(bytes);
            }
        });
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn capture_logs() {
    static SUBSCRIBER: Once = Once::new();
    // Share callsite interest across parallel tests and capture only the
    // logs emitted on the thread of the current-thread Tokio test.
    SUBSCRIBER.call_once(|| {
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .json()
                .with_writer(|| LogWriter)
                .finish(),
        )
        .unwrap();
    });
    LOG_OUTPUT.set(Some(Vec::new()));
}

fn captured_logs() -> String {
    String::from_utf8(LOG_OUTPUT.take().unwrap()).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn rejection_logs_retain_diagnostics_without_disclosing_source_text() {
    use sea_orm::{ConnectionTrait, Database};

    let fixture = Fixture::new().await;
    capture_logs();
    let response = fixture
        .request(
            Request::get("/api/exams")
                .header(header::COOKIE, "connect.sid=upstream-json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body(response).await,
        json!({"error": {
            "code": error_code::ACCODING_UNAVAILABLE,
            "message": "Accoding identity service unavailable"
        }})
    );

    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut socket = open_socket(&fixture, &state, 0).await;
    let db = Database::connect(&fixture.database_url).await.unwrap();
    db.execute_unprepared("CREATE TRIGGER reject_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'private test source text'); END")
        .await.unwrap();
    send(&mut socket, &navigation(1, "private test event detail")).await;
    assert_eq!(
        message(&mut socket).await,
        ServerMessage::Error {
            code: error_code::INTERNAL_ERROR.into(),
            message: "Internal server error".into(),
        }
    );

    let output = captured_logs();
    let entries: Vec<_> = output
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect();
    assert!(
        entries.iter().any(|entry| {
            let fields = &entry["fields"];
            fields["code"] == error_code::ACCODING_UNAVAILABLE
                && fields["reason"] == "invalid Accoding JSON response (Data)"
        }),
        "{output}"
    );
    assert!(
        entries.iter().any(|entry| {
            let fields = &entry["fields"];
            fields["code"] == error_code::INTERNAL_ERROR
                && fields["reason"].as_str().is_some_and(|reason| {
                    reason.starts_with("SQLite ") && reason.contains("code 1811")
                })
        }),
        "{output}"
    );
    assert!(!output.contains("connect.sid=upstream-json"));
    assert!(!output.contains("private test"));
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

#[tokio::test]
async fn revoking_enrollment_rejects_events_on_the_existing_socket() {
    let fixture = Fixture::new().await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut socket = open_socket(&fixture, &state, 0).await;
    send(&mut socket, &navigation(1, "before")).await;
    close_socket(&fixture, &mut socket).await;
    let mut socket = open_socket(&fixture, &state, 1).await;
    fixture
        .admin(AdminOperation::ParticipantsRemove {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await;
    socket_error(&mut socket, error_code::NOT_ENROLLED).await;

    fixture
        .admin(AdminOperation::ParticipantsAdd {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await;
    let mut socket = open_socket(&fixture, &state, 1).await;
    fixture
        .admin(AdminOperation::ParticipantsRemove {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await;
    socket_error(&mut socket, error_code::NOT_ENROLLED).await;
    fixture
        .admin(AdminOperation::ParticipantsAdd {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await;
    let mut socket = open_socket(&fixture, &state, 1).await;
    send(&mut socket, &navigation(2, "after re-enrolling")).await;
    close_socket(&fixture, &mut socket).await;
    assert_eq!(events(&fixture).await.len(), 2);
}

#[tokio::test]
async fn sqlite_contention_keeps_a_second_socket_open() {
    use sea_orm::{ConnectionTrait, Database, TransactionTrait};
    use tokio::time::timeout;

    let fixture = Fixture::new().await;
    fixture
        .admin(AdminOperation::ParticipantsAdd {
            exam_id: fixture.exam_id,
            oj_user_id: OTHER_STUDENT,
        })
        .await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut first = open_socket(&fixture, &state, 0).await;
    let mut other = fixture
        .upgrade(&fixture.path(), "connect.sid=other")
        .await
        .unwrap();
    send(&mut other, &state).await;
    assert!(matches!(
        message(&mut other).await,
        ServerMessage::Sync { .. }
    ));

    let writer = Database::connect(&fixture.database_url).await.unwrap();
    let lock = writer.begin().await.unwrap();
    lock.execute_unprepared("UPDATE exams SET title = title")
        .await
        .unwrap();
    close_socket(&fixture, &mut first).await;

    send(&mut other, &ClientMessage::Heartbeat).await;
    send(&mut other, &navigation(1, "during lock")).await;
    timeout(TIMEOUT, fixture.write_retries.notified())
        .await
        .unwrap();
    lock.rollback().await.unwrap();

    // Wait for the retried seq=1 event to commit before pipelining seq=2 and closing.
    tokio::time::sleep(Duration::from_millis(80)).await;
    send(&mut other, &navigation(2, "after unlock")).await;
    close_socket(&fixture, &mut other).await;
    let stored = fixture
        .admin(AdminOperation::EventsList {
            exam_id: fixture.exam_id,
            oj_user_id: OTHER_STUDENT,
        })
        .await;
    assert!(
        matches!(stored, AdminOutput::Events(rows) if rows.iter().map(|row| row.seq).eq([1, 2]))
    );
    fixture.state.hub.trigger_schedule().await;
    fixture.state.hub.barrier().await;
    assert_eq!(
        incidents(&fixture)
            .await
            .iter()
            .filter(|incident| incident.kind == "connection_lost" && incident.oj_user_id == STUDENT)
            .count(),
        1
    );
}

#[tokio::test]
async fn closing_during_sqlite_contention_discards_the_blocked_event() {
    use sea_orm::{ConnectionTrait, Database, TransactionTrait};
    use tokio::time::{self, timeout};

    let fixture = Fixture::new().await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut first = open_socket(&fixture, &state, 0).await;
    let writer = Database::connect(&fixture.database_url).await.unwrap();
    let lock = writer.begin().await.unwrap();
    lock.execute_unprepared("UPDATE exams SET title = title")
        .await
        .unwrap();
    time::pause();
    // Freeze retry backoff while the real SQLite worker and socket finish.
    // The watchdog allows virtual timers to advance if the close stalls.
    let watchdog = std::time::Instant::now() + TIMEOUT;
    let keep_clock_paused = tokio::spawn(async move {
        while std::time::Instant::now() < watchdog {
            tokio::task::yield_now().await;
        }
    });
    let batch_waiting = fixture.batch_waiting.notified();
    tokio::pin!(batch_waiting);
    batch_waiting.as_mut().enable();
    send(&mut first, &navigation(1, "abandoned event")).await;
    timeout(TIMEOUT, batch_waiting).await.unwrap();
    time::advance(Duration::from_millis(55)).await;
    timeout(TIMEOUT, fixture.write_retries.notified())
        .await
        .unwrap();

    // Observe the close with the write lock still held, before another retry.
    timeout(TIMEOUT, async {
        tokio::select! {
            biased;
            () = fixture.write_retries.notified() => panic!("write retried after close"),
            () = close_socket(&fixture, &mut first) => {}
        }
    })
    .await
    .unwrap();
    keep_clock_paused.abort();
    time::resume();
    assert!(incidents(&fixture).await.is_empty());
    lock.rollback().await.unwrap();

    fixture.clock.store(START, Ordering::Relaxed);
    let mut replacement = open_socket(&fixture, &state, 0).await;
    let start = command(&mut replacement, CommandName::StartExam, START).await;
    let event = navigation(1, "replacement event");
    send(&mut replacement, &event).await;
    confirm(&mut replacement, start).await;
    fixture.state.hub.trigger_schedule().await;
    fixture.state.hub.barrier().await;
    let ClientMessage::Event { event, .. } = event else {
        unreachable!();
    };
    assert_eq!(
        events(&fixture)
            .await
            .into_iter()
            .map(|row| row.event)
            .collect::<Vec<_>>(),
        [event]
    );
    let lost: Vec<_> = incidents(&fixture)
        .await
        .into_iter()
        .filter(|item| item.kind == "connection_lost")
        .collect();
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].detail.as_deref(), Some("closed"));
    assert_eq!(lost[0].occurred_at, instant(START - 1));
    let runtime = fixture
        .admin(AdminOperation::StudentsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await;
    assert!(matches!(runtime, AdminOutput::Student(runtime) if runtime.connected));
}

#[tokio::test]
async fn persistent_sqlite_contention_exhausts_the_retry_budget() {
    use sea_orm::{ConnectionTrait, Database, TransactionTrait};

    let fixture = Fixture::new().await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut socket = open_socket(&fixture, &state, 0).await;
    let writer = Database::connect(&fixture.database_url).await.unwrap();
    let lock = writer.begin().await.unwrap();
    lock.execute_unprepared("UPDATE exams SET title = title")
        .await
        .unwrap();
    send(&mut socket, &navigation(1, "blocked event")).await;
    tokio::time::timeout(
        Duration::from_secs(5),
        socket_error(&mut socket, error_code::INTERNAL_ERROR),
    )
    .await
    .unwrap();

    lock.rollback().await.unwrap();
    fixture.state.hub.trigger_schedule().await;
    fixture.state.hub.barrier().await;
    assert!(events(&fixture).await.is_empty());
    let lost = incidents(&fixture).await;
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].kind, "connection_lost");
}

#[tokio::test]
async fn disconnect_storage_failure_is_reconciled() {
    use sea_orm::{ConnectionTrait, Database};

    let fixture = Fixture::new().await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut socket = open_socket(&fixture, &state, 0).await;
    let db = Database::connect(&fixture.database_url).await.unwrap();
    db.execute_unprepared("CREATE TRIGGER reject_disconnect BEFORE UPDATE OF connected ON student_runtimes WHEN NEW.connected = 0 BEGIN SELECT RAISE(ABORT, 'test failure'); END")
        .await.unwrap();
    close_socket(&fixture, &mut socket).await;
    let mut replacement = fixture.upgrade(&fixture.path(), COOKIE).await.unwrap();
    send(&mut replacement, &state).await;
    socket_error(&mut replacement, error_code::INTERNAL_ERROR).await;
    db.execute_unprepared("DROP TRIGGER reject_disconnect")
        .await
        .unwrap();
    fixture.state.hub.trigger_schedule().await;
    fixture.state.hub.barrier().await;
    let stored = fixture
        .admin(AdminOperation::StudentsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await;
    assert!(matches!(stored, AdminOutput::Student(runtime) if !runtime.connected));
    assert_eq!(incidents(&fixture).await.len(), 1);
}

#[tokio::test]
async fn enforces_the_same_frame_and_message_limit_for_short_and_long_exams() {
    const LIMIT: usize = 65_536;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let padded = |size| {
        let mut bytes = serde_json::to_vec(&state).unwrap();
        bytes.resize(size, b' ');
        bytes
    };
    for end in [END, START + 4 * 3_600] {
        let fixture = Fixture::with_exam(end, 1).await;
        let mut socket = fixture.upgrade(&fixture.path(), COOKIE).await.unwrap();
        socket
            .send(Message::Text(
                String::from_utf8(padded(LIMIT)).unwrap().into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            message(&mut socket).await,
            ServerMessage::Sync { .. }
        ));
        close_socket(&fixture, &mut socket).await;

        let mut socket = fixture.upgrade(&fixture.path(), COOKIE).await.unwrap();
        socket
            .send(Message::Text(
                String::from_utf8(padded(LIMIT + 1)).unwrap().into(),
            ))
            .await
            .unwrap();
        socket_error(&mut socket, error_code::PROTOCOL_ERROR).await;

        let mut socket = fixture.upgrade(&fixture.path(), COOKIE).await.unwrap();
        let mut first = padded(LIMIT + 1);
        let last = first.split_off(LIMIT / 2);
        socket
            .send(Message::Frame(Frame::message(
                first,
                OpCode::Data(Data::Text),
                false,
            )))
            .await
            .unwrap();
        socket
            .send(Message::Frame(Frame::message(
                last,
                OpCode::Data(Data::Continue),
                true,
            )))
            .await
            .unwrap();
        socket_error(&mut socket, error_code::PROTOCOL_ERROR).await;
    }
}

#[tokio::test]
async fn accepts_a_two_hour_recording_summary_with_one_second_segments() {
    let end = START + 7_200;
    let fixture = Fixture::with_exam(end, 1).await;
    fixture.clock.store(end, Ordering::Relaxed);
    let state = hello(ExecutionState::Finalizing, RecordingState::Complete);
    let mut socket = open_socket(&fixture, &state, 0).await;
    let id = command(&mut socket, CommandName::EndExam, end).await;
    let digest = Sha256Digest::parse(&"b".repeat(64)).unwrap();
    let summary = RecordingSummary::new(7_200, digest).unwrap();
    let event = ClientMessage::Event {
        seq: 1,
        event: ExamEvent::RecordingFinalized(summary.clone()),
    };
    send(&mut socket, &event).await;
    confirm(&mut socket, id).await;
    let stored = fixture
        .admin(AdminOperation::RecordingsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await;
    assert!(matches!(stored, AdminOutput::Recording(found) if found == summary));
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
            event: ExamEvent::SegmentFinalized(segment),
        },
    )
    .await;

    fixture.clock.store(END, Ordering::Relaxed);
    fixture.state.hub.trigger_schedule().await;
    let end_command = command(&mut socket, CommandName::EndExam, END).await;
    assert_ne!(start_command, end_command);
    let summary = RecordingSummary::new(1, digest).unwrap();
    send(
        &mut socket,
        &ClientMessage::Event {
            seq: 2,
            event: ExamEvent::RecordingFinalized(summary.clone()),
        },
    )
    .await;
    confirm(&mut socket, end_command).await;
    socket.send(Message::Close(None)).await.unwrap();
    let _ = socket.next().await;
    fixture.state.hub.barrier().await;

    assert_finished_recording(&fixture, &summary).await;
    assert!(incidents(&fixture).await.is_empty());
}

async fn assert_finished_recording(fixture: &Fixture, summary: &RecordingSummary) {
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
    assert!(matches!(
        &events[0].event,
        ExamEvent::SegmentFinalized(segment) if segment.index == 1
    ));
    assert!(matches!(
        &events[1].event,
        ExamEvent::RecordingFinalized(found) if found == summary
    ));
    let AdminOutput::Recording(recording) = fixture
        .admin(AdminOperation::RecordingsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected recording");
    };
    assert_eq!(&recording, summary);
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

fn navigation(seq: i64, detail: &str) -> ClientMessage {
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
    ack_event_through: i64,
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
async fn occupied_port_preserves_the_existing_exam_connection() {
    let fixture = Fixture::new().await;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    fixture.clock.store(START, Ordering::Relaxed);
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut socket = open_socket(&fixture, &state, 0).await;
    let start = command(&mut socket, CommandName::StartExam, START).await;

    let startup = serve(
        ServerConfig {
            listen_addr: listener.local_addr().unwrap(),
            database_url: fixture.database_url.clone(),
            heartbeat_timeout_secs: TIMEOUT.as_secs(),
            tuning: crate::ServerTuning::default(),
        },
        None,
    )
    .await;
    assert!(
        matches!(startup, Err(ServerError::Io(error)) if error.kind() == io::ErrorKind::AddrInUse)
    );
    assert!(incidents(&fixture).await.is_empty());

    send(&mut socket, &navigation(1, "blocked")).await;
    confirm(&mut socket, start).await;
    assert_eq!(
        events(&fixture)
            .await
            .iter()
            .map(|row| row.seq)
            .collect::<Vec<_>>(),
        [1]
    );
    assert!(
        incidents(&fixture)
            .await
            .iter()
            .all(|row| row.kind != "connection_lost")
    );
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

async fn reconnect_after_runtime_loss() -> (Fixture, TestSocket, ExamEvent) {
    let fixture = Fixture::new().await;
    fixture.clock.store(START, Ordering::Relaxed);
    let ClientMessage::Hello(mut client) = hello(ExecutionState::Ready, RecordingState::Idle)
    else {
        unreachable!()
    };
    let mut socket = open_socket(&fixture, &ClientMessage::Hello(client.clone()), 0).await;
    let first_command = command(&mut socket, CommandName::StartExam, START).await;
    confirm(&mut socket, first_command).await;
    let original = ExamEvent::SegmentFinalized(
        SegmentMeta::new(
            1,
            instant(START),
            instant(START + 15),
            Sha256Digest::parse(&"a".repeat(64)).unwrap(),
        )
        .unwrap(),
    );
    send(
        &mut socket,
        &ClientMessage::Event {
            seq: 1,
            event: original.clone(),
        },
    )
    .await;
    close_socket(&fixture, &mut socket).await;
    let AdminOutput::Student(previous) = fixture
        .admin(AdminOperation::StudentsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected student");
    };
    assert!(!previous.connected);
    assert_eq!(previous.execution, ExecutionState::Recording);

    // The same device starts a new process without its saved execution state.
    client.instance_id = Uuid::new_v4();
    fixture.clock.store(START + 30, Ordering::Relaxed);
    let mut socket = fixture.upgrade(&fixture.path(), COOKIE).await.unwrap();
    send(&mut socket, &ClientMessage::Hello(client)).await;
    assert_eq!(
        message(&mut socket).await,
        ServerMessage::Sync {
            server_time: instant(START + 30),
            phase: ExamPhase::Running,
            start_at: instant(START),
            end_at: instant(END),
            recording_policy: RecordingPolicy::new(30, 15).unwrap(),
            ack_event_through: 1,
        }
    );
    let restarted = command(&mut socket, CommandName::StartExam, START).await;
    assert_ne!(restarted, first_command);
    confirm(&mut socket, restarted).await;
    let mut kinds = incidents(&fixture)
        .await
        .into_iter()
        .map(|incident| incident.kind)
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
    (fixture, socket, original)
}

#[tokio::test]
async fn runtime_loss_reconnects_ready_and_continues_recording() {
    let (fixture, mut socket, original) = reconnect_after_runtime_loss().await;
    let next = ExamEvent::SegmentFinalized(
        SegmentMeta::new(
            2,
            instant(START + 30),
            instant(START + 45),
            Sha256Digest::parse(&"b".repeat(64)).unwrap(),
        )
        .unwrap(),
    );
    send(
        &mut socket,
        &ClientMessage::Event {
            seq: 2,
            event: next.clone(),
        },
    )
    .await;
    close_socket(&fixture, &mut socket).await;
    assert_eq!(
        events(&fixture)
            .await
            .into_iter()
            .map(|event| (event.seq, event.event))
            .collect::<Vec<_>>(),
        [(1, original), (2, next)]
    );
    let state = hello(ExecutionState::Recording, RecordingState::Capturing);
    let mut socket = open_socket(&fixture, &state, 2).await;
    fixture.clock.store(END, Ordering::Relaxed);
    fixture.state.hub.trigger_schedule().await;
    command(&mut socket, CommandName::EndExam, END).await;
    close_socket(&fixture, &mut socket).await;
}

#[tokio::test]
async fn runtime_loss_with_reset_sequence_rejects_conflicting_event() {
    let (fixture, mut socket, original) = reconnect_after_runtime_loss().await;
    let conflicting = ExamEvent::SegmentFinalized(
        SegmentMeta::new(
            2,
            instant(START + 30),
            instant(START + 45),
            Sha256Digest::parse(&"b".repeat(64)).unwrap(),
        )
        .unwrap(),
    );
    send(
        &mut socket,
        &ClientMessage::Event {
            seq: 1,
            event: conflicting,
        },
    )
    .await;
    socket_error(&mut socket, error_code::PROTOCOL_ERROR).await;
    fixture.state.hub.barrier().await;
    assert_eq!(
        events(&fixture)
            .await
            .into_iter()
            .map(|event| (event.seq, event.event))
            .collect::<Vec<_>>(),
        [(1, original)]
    );
    let state = hello(ExecutionState::Recording, RecordingState::Capturing);
    let mut socket = open_socket(&fixture, &state, 1).await;
    close_socket(&fixture, &mut socket).await;
}

#[tokio::test]
async fn replays_end_handshake_after_ack_loss() {
    let fixture = Fixture::new().await;
    let state = hello(ExecutionState::Finalizing, RecordingState::Complete);
    fixture.clock.store(END, Ordering::Relaxed);
    let mut socket = open_socket(&fixture, &state, 0).await;
    let previous = command(&mut socket, CommandName::EndExam, END).await;
    let digest = Sha256Digest::parse(&"b".repeat(64)).unwrap();
    let summary = RecordingSummary::new(1, digest).unwrap();
    let finalized = ClientMessage::Event {
        seq: 1,
        event: ExamEvent::RecordingFinalized(summary),
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
    confirm(&mut resumed, current).await;
    confirm(&mut resumed, current).await;
    send(&mut resumed, &result).await;
    socket_error(&mut resumed, error_code::COMMAND_STATE_CONFLICT).await;
    assert!(incidents(&fixture).await.is_empty());
    assert_eq!(events(&fixture).await.len(), 1);
}

#[tokio::test]
async fn successful_end_waits_for_a_contiguous_recording_summary() {
    let fixture = Fixture::new().await;
    fixture.clock.store(END, Ordering::Relaxed);
    let state = hello(ExecutionState::Finalizing, RecordingState::Complete);
    let mut socket = open_socket(&fixture, &state, 0).await;
    let end = command(&mut socket, CommandName::EndExam, END).await;
    let digest = Sha256Digest::parse(&"b".repeat(64)).unwrap();
    let segment = SegmentMeta::new(1, instant(START), instant(END), digest.clone()).unwrap();
    let summary = RecordingSummary::new(1, digest).unwrap();
    send(
        &mut socket,
        &ClientMessage::Event {
            seq: 2,
            event: ExamEvent::RecordingFinalized(summary.clone()),
        },
    )
    .await;
    send(
        &mut socket,
        &ClientMessage::CommandResult {
            command_id: end,
            status: CommandStatus::Completed,
            error_code: None,
        },
    )
    .await;
    socket_error(&mut socket, error_code::COMMAND_STATE_CONFLICT).await;

    let mut socket = open_socket(&fixture, &state, 0).await;
    let end = command(&mut socket, CommandName::EndExam, END).await;
    send(
        &mut socket,
        &ClientMessage::Event {
            seq: 1,
            event: ExamEvent::SegmentFinalized(segment),
        },
    )
    .await;
    confirm(&mut socket, end).await;
    close_socket(&fixture, &mut socket).await;
    assert_finished_recording(&fixture, &summary).await;
}

#[tokio::test]
async fn replacement_ignores_old_close_timeout_and_event_notices() {
    use cvbbmso_domain::DisconnectReason;
    use tokio::time::Instant;

    let fixture = Fixture::new().await;
    let previous = TestPeer::connect(
        &fixture,
        STUDENT,
        ExecutionState::Recording,
        RecordingState::Capturing,
        0,
    )
    .await;

    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut current = open_socket(&fixture, &state, 0).await;
    assert!(previous.closing.await.unwrap().is_none());
    fixture
        .state
        .hub
        .end(previous.connection, DisconnectReason::Closed, None)
        .await;
    fixture
        .state
        .hub
        .end(
            previous.connection,
            DisconnectReason::HeartbeatTimeout,
            None,
        )
        .await;
    assert!(
        fixture
            .state
            .hub
            .receive(
                previous.connection,
                navigation(1, "stale"),
                Instant::now(),
                || {}
            )
            .await
            .is_none()
    );
    fixture.state.hub.barrier().await;
    assert!(events(&fixture).await.is_empty());
    let mut incidents = incidents(&fixture).await;
    incidents.sort_by(|left, right| left.kind.cmp(&right.kind));
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
    use cvbbmso_domain::DisconnectReason;
    use sea_orm::{ConnectionTrait, Database, TransactionTrait};
    use tokio::sync::oneshot;
    use tokio::time::{self, Instant};

    let fixture = Fixture::new().await;
    time::pause();
    // SQLite performs work on another thread. Keep Tokio from advancing time
    // automatically while that work is in flight.
    let keep_clock_paused = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let mut peer = TestPeer::connect(
        &fixture,
        STUDENT,
        ExecutionState::Ready,
        RecordingState::Idle,
        0,
    )
    .await;
    let original_deadline = peer.deadline;

    time::advance(TIMEOUT.checked_sub(Duration::from_secs(1)).unwrap()).await;
    let received_at = Instant::now();
    assert!(
        fixture
            .state
            .hub
            .receive(
                peer.connection,
                ClientMessage::Heartbeat,
                received_at,
                || {}
            )
            .await
            .is_some()
    );
    fixture.state.hub.barrier().await;
    time::advance(original_deadline - Instant::now()).await;
    fixture
        .state
        .hub
        .end(peer.connection, DisconnectReason::HeartbeatTimeout, None)
        .await;
    fixture.state.hub.barrier().await;
    assert!(matches!(
        peer.closing.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(incidents(&fixture).await.is_empty());

    let writer = Database::connect(&fixture.database_url).await.unwrap();
    let lock = writer.begin().await.unwrap();
    lock.execute_unprepared("UPDATE exams SET title = title")
        .await
        .unwrap();
    let hub = fixture.state.hub.clone();
    let connection = peer.connection;
    let batch_waiting = fixture.batch_waiting.notified();
    tokio::pin!(batch_waiting);
    batch_waiting.as_mut().enable();
    let pending = tokio::spawn(async move {
        hub.receive(
            connection,
            navigation(1, "locked at deadline"),
            Instant::now(),
            || {},
        )
        .await
    });
    batch_waiting.await;
    time::advance(Duration::from_millis(55)).await;
    fixture.write_retries.notified().await;
    time::advance(received_at + TIMEOUT - Instant::now()).await;
    tokio::select! {
        closed = &mut peer.closing => assert!(closed.unwrap().is_none()),
        () = fixture.write_retries.notified() => panic!("retried past the heartbeat deadline"),
    }
    assert!(pending.await.unwrap().is_none());
    lock.rollback().await.unwrap();
    fixture.state.hub.trigger_schedule().await;
    fixture
        .state
        .hub
        .end(peer.connection, DisconnectReason::HeartbeatTimeout, None)
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
        .direct_admin(AdminOperation::StudentsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected student");
    };
    assert!(!runtime.connected);
    let AdminOutput::Incidents(incidents_before_connect) = fixture
        .direct_admin(AdminOperation::IncidentsList {
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
    assert_eq!(incidents_before_connect.len(), 1);
    assert_eq!(incidents_before_connect[0].kind, "connection_lost");
    assert_eq!(
        incidents_before_connect[0].detail.as_deref(),
        Some("server_restart")
    );
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
    let fixture = Fixture::new().await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let peer = TestPeer::connect(
        &fixture,
        STUDENT,
        ExecutionState::Ready,
        RecordingState::Idle,
        0,
    )
    .await;
    drop(peer.outgoing);

    fixture.clock.store(START, Ordering::Relaxed);
    fixture.state.hub.trigger_schedule().await;
    assert!(peer.closing.await.unwrap().is_none());
    let incidents = incidents(&fixture).await;
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].detail.as_deref(), Some("closed"));

    let mut socket = open_socket(&fixture, &state, 0).await;
    let command_id = command(&mut socket, CommandName::StartExam, START).await;
    confirm(&mut socket, command_id).await;
}

#[tokio::test(flavor = "current_thread")]
async fn request_logs_include_metadata_and_keep_cookie_values_private() {
    let fixture = Fixture::new().await;
    capture_logs();
    let response = fixture
        .request(
            Request::get("/api/exams")
                .header(header::COOKIE, COOKIE)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let output = captured_logs();
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

#[tokio::test]
async fn heartbeat_writeback_cache_overlays_live_state_and_flushes_to_db() {
    let fixture = Fixture::new().await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    // Keep exam in Scheduled phase during trigger_schedule so no StartExam frame
    // races with close_socket's Close frame.
    fixture.clock.store(START - 5, Ordering::Relaxed);
    let mut socket = open_socket(&fixture, &state, 0).await;

    fixture.clock.store(START - 3, Ordering::Relaxed);
    send(&mut socket, &ClientMessage::Heartbeat).await;
    // Wait until Hub has processed the heartbeat into live state.
    let mut live = None;
    for _ in 0..50 {
        let AdminOutput::Student(student) = fixture
            .admin(AdminOperation::StudentsShow {
                exam_id: fixture.exam_id,
                oj_user_id: STUDENT,
            })
            .await
        else {
            panic!("expected student");
        };
        if student.last_seen_at == Some(instant(START - 3)) {
            live = Some(student);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let live = live.expect("heartbeat should be visible in live Hub state");
    assert!(live.connected);

    // Direct DB query only sees it after schedule tick flushes the write-back cache.
    let AdminOutput::Student(before_flush) = fixture
        .direct_admin(AdminOperation::StudentsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected student");
    };
    assert_eq!(before_flush.last_seen_at, Some(instant(START - 5)));

    fixture.state.hub.trigger_schedule().await;
    let AdminOutput::Student(after_flush) = fixture
        .direct_admin(AdminOperation::StudentsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected student");
    };
    assert_eq!(after_flush.last_seen_at, Some(instant(START - 3)));

    // A newer heartbeat not yet flushed is merged into Disconnect on close.
    fixture.clock.store(START - 2, Ordering::Relaxed);
    send(&mut socket, &ClientMessage::Heartbeat).await;
    for _ in 0..50 {
        let AdminOutput::Student(student) = fixture
            .admin(AdminOperation::StudentsShow {
                exam_id: fixture.exam_id,
                oj_user_id: STUDENT,
            })
            .await
        else {
            panic!("expected student");
        };
        if student.last_seen_at == Some(instant(START - 2)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    close_socket(&fixture, &mut socket).await;

    let AdminOutput::Student(after_close) = fixture
        .direct_admin(AdminOperation::StudentsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected student");
    };
    assert!(!after_close.connected);
    assert_eq!(after_close.last_seen_at, Some(instant(START - 2)));
}

#[tokio::test]
async fn group_commit_batches_concurrent_students() {
    let fixture = Fixture::new().await;
    fixture
        .admin(AdminOperation::ParticipantsAdd {
            exam_id: fixture.exam_id,
            oj_user_id: OTHER_STUDENT,
        })
        .await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut first = open_socket(&fixture, &state, 0).await;
    let mut second = fixture
        .upgrade(&fixture.path(), "connect.sid=other")
        .await
        .unwrap();
    send(&mut second, &state).await;
    assert!(matches!(
        message(&mut second).await,
        ServerMessage::Sync { .. }
    ));

    // Send reliable events concurrently from two students so they group-commit together.
    send(&mut first, &navigation(1, "student 1")).await;
    send(&mut second, &navigation(1, "student 2")).await;
    close_socket(&fixture, &mut first).await;
    close_socket(&fixture, &mut second).await;

    assert_eq!(events(&fixture).await.len(), 1);
    let AdminOutput::Events(other_events) = fixture
        .admin(AdminOperation::EventsList {
            exam_id: fixture.exam_id,
            oj_user_id: OTHER_STUDENT,
        })
        .await
    else {
        panic!("expected events");
    };
    assert_eq!(other_events.len(), 1);
}

#[tokio::test]
async fn graceful_shutdown_flushes_heartbeats_and_disconnects() {
    let fixture = Fixture::new().await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut socket = open_socket(&fixture, &state, 0).await;
    fixture.clock.store(START + 15, Ordering::Relaxed);
    send(&mut socket, &ClientMessage::Heartbeat).await;
    for _ in 0..50 {
        let AdminOutput::Student(student) = fixture
            .admin(AdminOperation::StudentsShow {
                exam_id: fixture.exam_id,
                oj_user_id: STUDENT,
            })
            .await
        else {
            panic!("expected student");
        };
        if student.last_seen_at == Some(instant(START + 15)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    fixture.state.hub.shutdown().await;
    let reopened = AdminService::open(&fixture.database_url).await.unwrap();
    let AdminOutput::Student(runtime) = reopened
        .execute(
            "staff".into(),
            AdminOperation::StudentsShow {
                exam_id: fixture.exam_id,
                oj_user_id: STUDENT,
            },
        )
        .await
        .unwrap()
    else {
        panic!("expected student");
    };
    assert!(!runtime.connected);
    assert_eq!(runtime.last_seen_at, Some(instant(START + 15)));
}

#[tokio::test]
async fn write_budget_tracks_pending_payload_bytes() {
    use crate::writer::WriteBudget;

    let budget = WriteBudget::new(2, 100);
    assert_eq!(budget.snapshot(), (0, 0));
    let p1 = budget.try_acquire(60).unwrap();
    assert_eq!(budget.snapshot(), (1, 60));
    assert!(budget.try_acquire(50).is_none());
    let p2 = budget.try_acquire(40).unwrap();
    assert_eq!(budget.snapshot(), (2, 100));
    assert!(budget.try_acquire(1).is_none());
    drop(p1);
    assert_eq!(budget.snapshot(), (1, 40));
    drop(p2);
    assert_eq!(budget.snapshot(), (0, 0));
}

#[tokio::test]
async fn bounded_auth_concurrency_returns_service_unavailable_on_timeout() {
    let mut fixture = Fixture::new().await;
    fixture.state.auth_semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    fixture.state.auth_wait_timeout = Duration::from_millis(20);
    // Hold the single auth permit so incoming requests time out waiting.
    let _held = fixture.state.auth_semaphore.acquire().await.unwrap();
    let response = fixture
        .request(
            Request::get("/api/exams")
                .header(header::COOKIE, COOKIE)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body(response).await["error"]["code"],
        error_code::ACCODING_UNAVAILABLE
    );
}

#[tokio::test]
async fn independent_scheduler_task_dispatches_due_commands_over_read_pool() {
    let mut fixture = Fixture::new().await;
    let state = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut socket = open_socket(&fixture, &state, 0).await;

    let scheduler = tokio::spawn(fixture.state.hub.clone().schedule());
    fixture.tasks.push(scheduler);

    fixture.clock.store(START, Ordering::Relaxed);
    let start_id = tokio::time::timeout(
        Duration::from_secs(5),
        command(&mut socket, CommandName::StartExam, START),
    )
    .await
    .unwrap();
    confirm(&mut socket, start_id).await;
    close_socket(&fixture, &mut socket).await;
}

#[tokio::test]
async fn heartbeat_cache_ignores_stale_generation_after_replacement() {
    let fixture = Fixture::new().await;
    let first_hello = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut old_socket = open_socket(&fixture, &first_hello, 0).await;

    fixture.clock.store(START - 4, Ordering::Relaxed);
    send(&mut old_socket, &ClientMessage::Heartbeat).await;

    // Replace with a newer connection at START - 2.
    fixture.clock.store(START - 2, Ordering::Relaxed);
    let second_hello = hello(ExecutionState::Ready, RecordingState::Idle);
    let mut new_socket = open_socket(&fixture, &second_hello, 0).await;
    assert!(matches!(
        old_socket.next().await,
        Some(Ok(Message::Close(_))) | None
    ));

    fixture.state.hub.trigger_schedule().await;
    let AdminOutput::Student(runtime) = fixture
        .direct_admin(AdminOperation::StudentsShow {
            exam_id: fixture.exam_id,
            oj_user_id: STUDENT,
        })
        .await
    else {
        panic!("expected student");
    };
    assert!(runtime.connected);
    assert_eq!(runtime.last_seen_at, Some(instant(START - 2)));
    close_socket(&fixture, &mut new_socket).await;
}
