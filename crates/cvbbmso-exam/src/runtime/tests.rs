//! Runtime behavior against a loopback monitor and a scripted recorder.

// tungstenite fixes the handshake callback's large `ErrorResponse` error type.
#![allow(clippy::result_large_err)]

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use cvbbmso_accoding::AccodingCookies;
use cvbbmso_protocol::{
    ClientMessage, ClientState, CommandName, CommandStatus, ExamEvent, ExamPhase, ExecutionState,
    IncidentCode, RecordingPolicy, RecordingState, RecordingSummary, SegmentMeta, ServerMessage,
    Sha256Digest, error_code,
};
use cvbbmso_recorder::{RecorderEvent, RecordingError};
use futures_util::{SinkExt, StreamExt};
use time::OffsetDateTime;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        http::StatusCode,
    },
};
use url::Url;
use uuid::Uuid;

use super::{Capture, Clock, END_GRACE, Launch, Runtime, Timing, launch, session::Liveness};
use crate::{
    Exam, ExamConfig, ExamError, ExamHandle, UiNotice, journal::Journal, monitor::Monitor,
};

const EXAM: Uuid = Uuid::from_u128(0x10);
const DEVICE: Uuid = Uuid::from_u128(0x20);
const INSTANCE: Uuid = Uuid::from_u128(0x30);
const USER: i64 = 42;
const SESSION: &str = "connect.sid=session";
const WAIT: Duration = Duration::from_secs(5);

#[test]
fn capture_diagnostics_keep_native_context_and_bound_remote_details() {
    let error = RecordingError::Platform(Some(std::io::Error::other(
        "reading desktop frame: access lost (0x887A0026)",
    )));
    assert!(
        super::recording_detail(&error).contains("reading desktop frame: access lost (0x887A0026)")
    );
    let long = RecordingError::Platform(Some(std::io::Error::other("失".repeat(400))));
    assert_eq!(
        super::recording_detail(&long).chars().count(),
        super::MAX_DETAIL_CHARS
    );
    let storage = RecordingError::Io(std::io::Error::other("private recording directory"));
    assert_eq!(
        super::recording_detail(&storage),
        "recording storage operation failed"
    );
}

fn timing() -> Timing {
    Timing {
        liveness: Liveness {
            heartbeat: Duration::from_secs(60),
            silence: Duration::from_secs(60),
        },
        reconnect_min: Duration::from_millis(20),
        reconnect_max: Duration::from_millis(100),
        retry: Duration::from_millis(20),
        retry_max: Duration::from_millis(20),
        start_attempts: 3,
        finish_attempts: 2,
        ack_timeout: Duration::from_secs(60),
    }
}

fn state(execution: ExecutionState, recording: RecordingState) -> ClientState {
    ClientState {
        execution,
        recording,
    }
}

fn segment(index: u32) -> SegmentMeta {
    let start = OffsetDateTime::UNIX_EPOCH + Duration::from_secs(u64::from(index));
    SegmentMeta::new(
        index,
        start,
        start,
        Sha256Digest::parse(&"a".repeat(64)).unwrap(),
    )
    .unwrap()
}

fn sync(phase: ExamPhase, ack_event_through: i64) -> ServerMessage {
    let now = OffsetDateTime::now_utc();
    ServerMessage::Sync {
        server_time: now,
        phase,
        start_at: now - Duration::from_secs(60),
        end_at: now + Duration::from_secs(3600),
        recording_policy: RecordingPolicy::new(30, 15).unwrap(),
        ack_event_through,
    }
}

fn command(id: u128, name: CommandName) -> ServerMessage {
    ServerMessage::Command {
        command_id: Uuid::from_u128(id),
        name,
        effective_at: OffsetDateTime::now_utc(),
    }
}

fn ack(id: u128) -> ServerMessage {
    ServerMessage::CommandAck {
        command_id: Uuid::from_u128(id),
    }
}

/// Recorder whose outcomes the test controls.
#[derive(Clone, Default)]
struct Script(Arc<Mutex<ScriptState>>);

#[derive(Default)]
struct ScriptState {
    calls: Vec<&'static str>,
    start_failures: u32,
    omit_segment_events: bool,
}

impl Script {
    fn fail_next_starts(&self, count: u32) {
        self.0.lock().unwrap().start_failures = count;
    }

    fn calls(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().calls.clone()
    }
}

struct FakeCapture {
    script: Script,
    segment_count: u32,
    recorder: mpsc::Sender<RecorderEvent>,
}

impl Capture for FakeCapture {
    fn start(&mut self) -> impl Future<Output = Result<(), RecordingError>> + Send {
        let mut script = self.script.0.lock().unwrap();
        script.calls.push("start");
        let result = if script.start_failures > 0 {
            script.start_failures -= 1;
            Err(RecordingError::Platform(None))
        } else {
            Ok(())
        };
        async move { result }
    }

    fn stop(&mut self) -> impl Future<Output = Result<(), RecordingError>> + Send {
        self.script.0.lock().unwrap().calls.push("stop");
        async { Ok(()) }
    }

    fn finish(&mut self) -> impl Future<Output = Result<RecordingSummary, RecordingError>> + Send {
        let omit_segment_events = {
            let mut script = self.script.0.lock().unwrap();
            script.calls.push("finish");
            script.omit_segment_events
        };
        let segment_count = self.segment_count;
        let recorder = self.recorder.clone();
        async move {
            if !omit_segment_events {
                for index in 1..=segment_count {
                    recorder
                        .send(RecorderEvent::SegmentFinalized(segment(index)))
                        .await
                        .unwrap();
                }
            }
            Ok(
                RecordingSummary::new(segment_count, Sha256Digest::parse(&"b".repeat(64)).unwrap())
                    .unwrap(),
            )
        }
    }
}

struct OpenGate {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

/// Loopback monitor, scripted recorder, and data directory of one test.
struct Harness {
    listener: TcpListener,
    base: Url,
    data: tempfile::TempDir,
    script: Script,
    recorder: mpsc::Sender<RecorderEvent>,
    events: Option<mpsc::Receiver<RecorderEvent>>,
    segment_count: u32,
    open_gate: Option<OpenGate>,
}

impl Harness {
    async fn new() -> Self {
        Self::with_event_capacity(16).await
    }

    async fn with_event_capacity(capacity: usize) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (recorder, events) = mpsc::channel(capacity);
        Self {
            listener,
            base,
            data: tempfile::tempdir().unwrap(),
            script: Script::default(),
            recorder,
            events: Some(events),
            segment_count: 1,
            open_gate: None,
        }
    }

    fn candidate_dir(&self) -> std::path::PathBuf {
        self.data
            .path()
            .join("exams")
            .join(EXAM.to_string())
            .join(USER.to_string())
    }

    fn block_journal_writes(&self) -> std::path::PathBuf {
        let path = self.candidate_dir().join("runtime").join("journal.json");
        std::fs::rename(&path, path.with_extension("saved")).unwrap();
        std::fs::create_dir(&path).unwrap();
        path
    }

    fn launch(
        &mut self,
        finalizing_user: Option<i64>,
    ) -> JoinHandle<Result<ExamHandle, ExamError>> {
        let config = ExamConfig {
            server_base: self.base.clone(),
            app_data_dir: self.data.path().to_owned(),
            device_id: DEVICE,
            instance_id: INSTANCE,
        };
        let cookies =
            AccodingCookies::from_pairs([("connect.sid".into(), "session".into())]).unwrap();
        let monitor = Monitor::for_test(self.base.clone()).unwrap();
        let capture = FakeCapture {
            script: self.script.clone(),
            segment_count: self.segment_count,
            recorder: self.recorder.clone(),
        };
        let events = self.events.take().expect("launched once per harness");
        let open_gate = self.open_gate.take();
        tokio::spawn(launch(
            Launch {
                config,
                exam_id: EXAM,
                cookies,
                monitor,
                timing: timing(),
                finalizing_user,
            },
            move |_, _, _| async move {
                if let Some(gate) = open_gate {
                    gate.entered.send(()).unwrap();
                    gate.release.await.unwrap();
                }
                Ok((capture, events))
            },
        ))
    }

    async fn begin(&mut self, phase: ExamPhase) -> JoinHandle<Result<ExamHandle, ExamError>> {
        let pending = self.launch((phase == ExamPhase::Ended).then_some(USER));
        self.answer_enrollment(SESSION, USER, phase).await;
        pending
    }

    async fn accept(&self) -> Conn {
        self.accept_with(|_, response| Ok(response)).await
    }

    async fn answer_enrollment(&self, expected_cookie: &str, user: i64, phase: ExamPhase) {
        timeout(WAIT, async {
            let (tcp, _) = self.listener.accept().await.unwrap();
            let mut tcp = BufReader::new(tcp);
            let mut line = String::new();
            tcp.read_line(&mut line).await.unwrap();
            assert_eq!(line, "GET /api/exams HTTP/1.1\r\n");
            let mut cookie = None;
            loop {
                line.clear();
                assert_ne!(tcp.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("cookie")
                {
                    cookie = Some(value.trim().to_owned());
                }
            }
            assert_eq!(cookie.as_deref(), Some(expected_cookie));

            let body = serde_json::json!({
                "user": { "id": user, "username": "candidate", "nickname": "Candidate" },
                "items": [{
                    "id": EXAM, "title": "Exam", "phase": phase,
                    "start_at": "2026-12-20T01:00:00Z", "end_at": "2026-12-20T04:00:00Z"
                }]
            }).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            tcp.get_mut().write_all(response.as_bytes()).await.unwrap();
        })
        .await
        .expect("runtime verifies enrollment over HTTP");
    }

    async fn accept_with(
        &self,
        decide: impl FnOnce(&Request, Response) -> Result<Response, ErrorResponse> + Unpin,
    ) -> Conn {
        let (tcp, _) = timeout(WAIT, self.listener.accept())
            .await
            .expect("client connects")
            .unwrap();
        let mut cookie = None;
        let accepted = accept_hdr_async(tcp, |request: &Request, response: Response| {
            cookie = request
                .headers()
                .get("cookie")
                .map(|value| value.to_str().unwrap().to_owned());
            decide(request, response)
        })
        .await;
        Conn {
            ws: accepted.ok(),
            cookie,
        }
    }

    /// Launches, accepts, answers `hello` with a `sync`, and returns the handle.
    async fn connected(&mut self, phase: ExamPhase) -> (Conn, ExamHandle) {
        let pending = self.begin(phase).await;
        let mut conn = self.accept().await;
        assert!(matches!(conn.recv().await, ClientMessage::Hello(_)));
        conn.send(sync(phase, 0)).await;
        let handle = pending.await.unwrap().unwrap();
        (conn, handle)
    }
}

struct Conn {
    ws: Option<WebSocketStream<TcpStream>>,
    cookie: Option<String>,
}

impl Conn {
    fn ws(&mut self) -> &mut WebSocketStream<TcpStream> {
        self.ws.as_mut().expect("handshake accepted")
    }

    /// Next protocol message, skipping heartbeats and control frames.
    async fn recv(&mut self) -> ClientMessage {
        loop {
            let frame = timeout(WAIT, self.ws().next())
                .await
                .expect("client message")
                .expect("open connection")
                .unwrap();
            if let Message::Text(text) = frame {
                let message: ClientMessage = serde_json::from_str(&text).unwrap();
                if message != ClientMessage::Heartbeat {
                    return message;
                }
            }
        }
    }

    async fn send(&mut self, message: ServerMessage) {
        let text = serde_json::to_string(&message).unwrap();
        self.ws().send(Message::text(text)).await.unwrap();
    }

    /// Waits until the client closes the connection.
    async fn closed(&mut self) {
        loop {
            match timeout(WAIT, self.ws().next())
                .await
                .expect("client closes")
            {
                None | Some(Ok(Message::Close(_)) | Err(_)) => return,
                Some(Ok(_)) => {}
            }
        }
    }
}

fn expect_status(message: &ClientMessage, expected: ClientState) {
    assert_eq!(*message, ClientMessage::Status { state: expected });
}

fn expect_result(message: &ClientMessage, id: u128, status: CommandStatus) {
    let ClientMessage::CommandResult {
        command_id,
        status: actual,
        ..
    } = message
    else {
        panic!("expected a command result, got {message:?}");
    };
    assert_eq!((*command_id, *actual), (Uuid::from_u128(id), status));
}

fn expect_event(message: ClientMessage) -> (i64, ExamEvent) {
    match message {
        ClientMessage::Event { seq, event } => (seq, event),
        other => panic!("expected an event, got {other:?}"),
    }
}

fn expect_incident(message: ClientMessage, code: IncidentCode) -> (i64, Option<String>) {
    match expect_event(message) {
        (seq, ExamEvent::Incident(incident)) if incident.code == code => (seq, incident.detail),
        other => panic!("expected a {code:?} incident, got {other:?}"),
    }
}

/// Starts recording and completes the `start_exam` handshake.
async fn start_recording(conn: &mut Conn) {
    conn.send(command(1, CommandName::StartExam)).await;
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Recording, RecordingState::Capturing),
    );
    expect_result(&conn.recv().await, 1, CommandStatus::Completed);
    conn.send(ack(1)).await;
}

#[tokio::test]
async fn connect_sends_hello_with_the_session_and_waits_for_sync() {
    let mut harness = Harness::new().await;
    let pending = harness.begin(ExamPhase::Scheduled).await;
    let mut conn = harness.accept().await;

    assert_eq!(conn.cookie.as_deref(), Some("connect.sid=session"));
    let ClientMessage::Hello(hello) = conn.recv().await else {
        panic!("hello must come first");
    };
    assert_eq!((hello.device_id, hello.instance_id), (DEVICE, INSTANCE));
    assert_eq!(
        hello.state,
        state(ExecutionState::Ready, RecordingState::Idle)
    );
    assert!(!pending.is_finished(), "connect returns only after sync");

    conn.send(sync(ExamPhase::Scheduled, 0)).await;
    let handle = pending.await.unwrap().unwrap();

    let view = handle.subscribe().borrow().clone();
    assert!(view.connected);
    assert_eq!(view.exam_id, EXAM);
}

#[tokio::test]
async fn commands_drive_the_recording_through_the_end_handshake() {
    let mut harness = Harness::new().await;
    let (mut conn, handle) = harness.connected(ExamPhase::Running).await;
    let mut view = handle.subscribe();

    conn.send(command(1, CommandName::StartExam)).await;
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Recording, RecordingState::Capturing),
    );
    expect_result(&conn.recv().await, 1, CommandStatus::Completed);
    harness
        .recorder
        .send(RecorderEvent::SegmentFinalized(segment(1)))
        .await
        .unwrap();
    let (seq, event) = expect_event(conn.recv().await);
    assert_eq!(seq, 1);
    assert!(matches!(event, ExamEvent::SegmentFinalized(meta) if meta.index == 1));

    // The end command can arrive before the start acknowledgement.
    conn.send(command(2, CommandName::EndExam)).await;
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Finalizing, RecordingState::Finalizing),
    );
    let (seq, event) = expect_event(conn.recv().await);
    assert_eq!(seq, 2);
    assert!(matches!(event, ExamEvent::RecordingFinalized(_)));
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Finalizing, RecordingState::Complete),
    );
    expect_result(&conn.recv().await, 2, CommandStatus::Completed);

    conn.send(ack(1)).await;
    conn.send(ack(2)).await;
    conn.closed().await;
    let finished = state(ExecutionState::Finished, RecordingState::Complete);
    timeout(
        WAIT,
        view.wait_for(|view| view.state == finished && !view.connected),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(harness.script.calls(), ["start", "finish"]);
}

#[tokio::test]
async fn reconnecting_resends_only_unconfirmed_events() {
    let mut harness = Harness::new().await;
    let (mut conn, _handle) = harness.connected(ExamPhase::Running).await;
    start_recording(&mut conn).await;
    for index in 1..=2 {
        harness
            .recorder
            .send(RecorderEvent::SegmentFinalized(segment(index)))
            .await
            .unwrap();
        assert_eq!(expect_event(conn.recv().await).0, i64::from(index));
    }

    drop(conn);
    let mut conn = harness.accept().await;
    let ClientMessage::Hello(hello) = conn.recv().await else {
        panic!("hello must come first");
    };
    assert_eq!(
        hello.state,
        state(ExecutionState::Recording, RecordingState::Capturing)
    );
    conn.send(sync(ExamPhase::Running, 1)).await;

    let (seq, event) = expect_event(conn.recv().await);
    assert_eq!(seq, 2);
    assert!(matches!(event, ExamEvent::SegmentFinalized(meta) if meta.index == 2));
}

#[tokio::test]
async fn a_reconnect_rejects_commands_before_sync() {
    let mut harness = Harness::new().await;
    let (conn, handle) = harness.connected(ExamPhase::Scheduled).await;
    drop(conn);

    let mut conn = harness.accept().await;
    assert!(matches!(conn.recv().await, ClientMessage::Hello(_)));
    conn.send(command(1, CommandName::StartExam)).await;
    conn.closed().await;
    assert_eq!(harness.script.calls(), Vec::<&str>::new());

    let mut conn = harness.accept().await;
    let ClientMessage::Hello(hello) = conn.recv().await else {
        panic!("hello must come first");
    };
    assert_eq!(
        hello.state,
        state(ExecutionState::Ready, RecordingState::Idle)
    );
    conn.send(sync(ExamPhase::Running, 0)).await;
    start_recording(&mut conn).await;
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_same_account_cookie_update_is_used_on_reconnect() {
    let mut harness = Harness::new().await;
    let (mut conn, handle) = harness.connected(ExamPhase::Running).await;
    let mut view = handle.subscribe();
    start_recording(&mut conn).await;

    conn.send(ServerMessage::Error {
        code: error_code::ACCODING_AUTH_EXPIRED.to_owned(),
        message: "Sign in again".to_owned(),
    })
    .await;
    timeout(
        WAIT,
        view.wait_for(|view| {
            view.last_error_code.as_deref() == Some(error_code::ACCODING_AUTH_EXPIRED)
        }),
    )
    .await
    .unwrap()
    .unwrap();

    let cookies =
        AccodingCookies::from_pairs([("connect.sid".into(), "refreshed".into())]).unwrap();
    handle
        .notify(UiNotice::CookiesUpdated(cookies))
        .await
        .unwrap();
    harness
        .answer_enrollment("connect.sid=refreshed", USER, ExamPhase::Running)
        .await;
    timeout(
        WAIT,
        view.wait_for(|view| view.connected && view.last_error_code.is_none()),
    )
    .await
    .unwrap()
    .unwrap();

    drop(conn);
    let mut conn = harness.accept().await;
    assert_eq!(conn.cookie.as_deref(), Some("connect.sid=refreshed"));
    let ClientMessage::Hello(hello) = conn.recv().await else {
        panic!("hello must come first");
    };
    assert_eq!(
        hello.state,
        state(ExecutionState::Recording, RecordingState::Capturing)
    );
    conn.send(sync(ExamPhase::Running, 0)).await;
    timeout(
        WAIT,
        view.wait_for(|view| view.connected && view.last_error_code.is_none()),
    )
    .await
    .unwrap()
    .unwrap();
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn another_enrolled_account_cannot_send_the_original_candidates_events() {
    let mut harness = Harness::new().await;
    let (mut conn, handle) = harness.connected(ExamPhase::Running).await;
    let mut view = handle.subscribe();
    start_recording(&mut conn).await;
    harness
        .recorder
        .send(RecorderEvent::SegmentFinalized(segment(1)))
        .await
        .unwrap();
    assert_eq!(expect_event(conn.recv().await).0, 1);

    let cookies = AccodingCookies::from_pairs([("connect.sid".into(), "other".into())]).unwrap();
    handle
        .notify(UiNotice::CookiesUpdated(cookies))
        .await
        .unwrap();
    harness
        .answer_enrollment("connect.sid=other", USER + 1, ExamPhase::Running)
        .await;
    timeout(
        WAIT,
        view.wait_for(|view| {
            view.connected
                && view.last_error_code.as_deref() == Some(error_code::ACCODING_AUTH_EXPIRED)
        }),
    )
    .await
    .unwrap()
    .unwrap();

    drop(conn);
    let mut conn = harness.accept().await;
    assert_eq!(conn.cookie.as_deref(), Some("connect.sid=session"));
    assert!(matches!(conn.recv().await, ClientMessage::Hello(_)));
    conn.send(sync(ExamPhase::Running, 0)).await;
    assert_eq!(expect_event(conn.recv().await).0, 1);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_capture_fault_is_reported_and_retried_until_it_recovers() {
    let mut harness = Harness::new().await;
    let (mut conn, _handle) = harness.connected(ExamPhase::Running).await;
    start_recording(&mut conn).await;
    harness.script.fail_next_starts(1);

    for _ in 0..2 {
        harness
            .recorder
            .try_send(RecorderEvent::Fault(RecordingError::Platform(None)))
            .unwrap();
    }

    expect_incident(conn.recv().await, IncidentCode::RecordingFailed);
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Recording, RecordingState::Failed),
    );
    // The first retry fails silently; the second recovers.
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Recording, RecordingState::Capturing),
    );
    expect_incident(conn.recv().await, IncidentCode::RecordingRecovered);
    assert_eq!(harness.script.calls(), ["start", "start", "start"]);
}

#[tokio::test]
async fn a_restarted_process_reports_the_lost_capture_and_resumes_it() {
    let mut harness = Harness::new().await;
    let mut journal = Journal::open(&harness.candidate_dir()).await.unwrap();
    journal.set_state(state(ExecutionState::Recording, RecordingState::Capturing));
    journal.save().await.unwrap();

    let pending = harness.begin(ExamPhase::Running).await;
    let mut conn = harness.accept().await;
    let ClientMessage::Hello(hello) = conn.recv().await else {
        panic!("hello must come first");
    };
    assert_eq!(
        hello.state,
        state(ExecutionState::Recording, RecordingState::Failed)
    );
    conn.send(sync(ExamPhase::Running, 0)).await;
    let _handle = pending.await.unwrap().unwrap();

    expect_status(
        &conn.recv().await,
        state(ExecutionState::Recording, RecordingState::Capturing),
    );
    expect_incident(conn.recv().await, IncidentCode::RecordingRecovered);
}

#[tokio::test]
async fn recorder_restoration_does_not_extend_the_server_deadline() {
    let mut harness = Harness::new().await;
    let mut journal = Journal::open(&harness.candidate_dir()).await.unwrap();
    journal.set_state(state(ExecutionState::Recording, RecordingState::Capturing));
    journal.save().await.unwrap();
    let (entered, restoring) = oneshot::channel();
    let (release, wait_for_release) = oneshot::channel();
    harness.open_gate = Some(OpenGate {
        entered,
        release: wait_for_release,
    });

    let pending = harness.begin(ExamPhase::Ended).await;
    let mut conn = harness.accept().await;
    assert!(matches!(conn.recv().await, ClientMessage::Hello(_)));
    let server_time = OffsetDateTime::UNIX_EPOCH + Duration::from_secs(120);
    conn.send(ServerMessage::Sync {
        server_time,
        phase: ExamPhase::Ended,
        start_at: OffsetDateTime::UNIX_EPOCH,
        end_at: server_time - END_GRACE + Duration::from_secs(10),
        recording_policy: RecordingPolicy::new(30, 15).unwrap(),
        ack_event_through: 0,
    })
    .await;
    timeout(WAIT, restoring).await.unwrap().unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    release.send(()).unwrap();

    let handle = pending.await.unwrap().unwrap();
    let mut view = handle.subscribe();
    timeout(
        Duration::from_secs(1),
        view.wait_for(|view| view.state.execution == ExecutionState::Finalizing),
    )
    .await
    .expect("the server has already reached the stop deadline")
    .unwrap();
    assert_eq!(harness.script.calls(), ["stop"]);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn repeated_navigation_denials_are_reported_once() {
    let mut harness = Harness::new().await;
    let (mut conn, handle) = harness.connected(ExamPhase::Running).await;

    for origin in [
        "https://a.example",
        "https://b.example",
        "https://a.example",
        "https://c.example",
    ] {
        handle
            .notify(UiNotice::NavigationDenied {
                target_origin: origin.into(),
            })
            .await
            .unwrap();
    }

    let (first, detail) = expect_incident(conn.recv().await, IncidentCode::NavigationDenied);
    assert_eq!((first, detail.as_deref()), (1, Some("https://a.example")));
    let (second, detail) = expect_incident(conn.recv().await, IncidentCode::NavigationDenied);
    assert_eq!((second, detail.as_deref()), (2, Some("https://b.example")));
    let (third, detail) = expect_incident(conn.recv().await, IncidentCode::NavigationDenied);
    assert_eq!((third, detail.as_deref()), (3, Some("https://c.example")));
}

#[tokio::test]
async fn shutdown_stops_capture_and_closes_the_connection() {
    let mut harness = Harness::new().await;
    let (mut conn, handle) = harness.connected(ExamPhase::Running).await;
    start_recording(&mut conn).await;

    handle.shutdown().await.unwrap();

    conn.closed().await;
    assert_eq!(harness.script.calls(), ["start", "stop"]);
}

#[tokio::test(start_paused = true)]
async fn a_full_send_queue_cannot_hold_recording_past_the_offline_deadline() {
    let harness = Harness::new().await;
    let mut journal = Journal::open(&harness.candidate_dir()).await.unwrap();
    let state = state(ExecutionState::Recording, RecordingState::Capturing);
    journal.set_state(state);
    journal.anchor(0);

    let (link, mut stalled_session) = mpsc::channel(1);
    link.try_send(ClientMessage::Heartbeat).unwrap();
    let (recorder_tx, recorder) = mpsc::channel(1);
    recorder_tx
        .try_send(RecorderEvent::SegmentFinalized(segment(1)))
        .unwrap();
    let (requests_tx, requests) = mpsc::channel(1);
    let (inbound_tx, inbound) = mpsc::channel(1);
    let (view_tx, mut view) = watch::channel(crate::ClientView {
        exam_id: EXAM,
        connected: true,
        state,
        last_error_code: None,
    });
    let server_time = OffsetDateTime::UNIX_EPOCH + Duration::from_secs(120);
    let timing = timing();
    let runtime = Runtime {
        exam_id: EXAM,
        oj_user_id: USER,
        device_id: DEVICE,
        instance_id: INSTANCE,
        monitor: Monitor::for_test(harness.base.clone()).unwrap(),
        cookies: AccodingCookies::from_pairs([("connect.sid".into(), "session".into())]).unwrap(),
        timing,
        journal,
        capture: FakeCapture {
            script: harness.script.clone(),
            segment_count: 1,
            recorder: harness.recorder.clone(),
        },
        recorder,
        inbound_tx,
        inbound,
        requests,
        view: view_tx,
        session: None,
        link: Some(link),
        generation: 1,
        connected: true,
        state,
        last_error_code: None,
        clock: Some(Clock {
            server_time,
            at: Instant::now(),
        }),
        end_at: Some(server_time - END_GRACE + Duration::from_secs(2)),
        command: None,
        reconnect_at: None,
        backoff: Duration::from_secs(60),
        retry_at: None,
        start_failures: 0,
        ack_deadline: None,
        recent_denials: std::collections::HashMap::new(),
        done: false,
        storage_retry_at: None,
        sent_through: 0,
    };
    let task = tokio::spawn(runtime.run());

    timeout(
        Duration::from_secs(3),
        view.wait_for(|view| !view.connected && view.state.execution == ExecutionState::Finalizing),
    )
    .await
    .expect("stop at the monotonic deadline")
    .unwrap();
    assert!(stalled_session.is_closed());
    assert_eq!(
        stalled_session.try_recv().unwrap(),
        ClientMessage::Heartbeat
    );
    assert_eq!(harness.script.calls(), ["stop"]);
    let journal = Journal::open(&harness.candidate_dir()).await.unwrap();
    assert_eq!(
        journal
            .messages_after(0)
            .map(|(seq, _)| seq)
            .collect::<Vec<_>>(),
        [1]
    );

    let handle = ExamHandle {
        requests: requests_tx,
        view,
        task,
    };
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_rejected_session_fails_connect_with_auth() {
    let mut harness = Harness::new().await;
    let pending = harness.begin(ExamPhase::Running).await;

    let conn = harness
        .accept_with(|_, _| {
            let mut response = ErrorResponse::new(None);
            *response.status_mut() = StatusCode::UNAUTHORIZED;
            Err(response)
        })
        .await;

    assert!(conn.ws.is_none());
    let error = pending.await.unwrap().unwrap_err();
    assert!(matches!(error, ExamError::Auth), "{error:?}");
}

#[tokio::test]
async fn a_failed_journal_write_keeps_the_exam_running() {
    let mut harness = Harness::new().await;
    let (mut conn, handle) = harness.connected(ExamPhase::Running).await;
    let mut view = handle.subscribe();
    start_recording(&mut conn).await;
    let blocker = harness.block_journal_writes();

    harness
        .recorder
        .send(RecorderEvent::SegmentFinalized(segment(1)))
        .await
        .unwrap();

    timeout(
        WAIT,
        view.wait_for(|view| view.last_error_code.as_deref() == Some("INTERNAL_ERROR")),
    )
    .await
    .unwrap()
    .unwrap();

    harness
        .recorder
        .send(RecorderEvent::SegmentFinalized(segment(2)))
        .await
        .unwrap();
    conn.send(command(2, CommandName::StartExam)).await;
    expect_result(&conn.recv().await, 2, CommandStatus::Completed);
    conn.send(ack(2)).await;

    std::fs::remove_dir(&blocker).unwrap();
    assert_eq!(expect_event(conn.recv().await).0, 1);
    assert_eq!(expect_event(conn.recv().await).0, 2);
    timeout(WAIT, view.wait_for(|view| view.last_error_code.is_none()))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn end_result_waits_for_the_tail_segment_and_summary_to_be_saved_and_sent() {
    // The fake fills the single slot with a duplicate, then waits to deliver
    // the tail before returning. The runtime must consume events during finish.
    let mut harness = Harness::with_event_capacity(1).await;
    harness.segment_count = 2;
    let (mut conn, handle) = harness.connected(ExamPhase::Running).await;
    let mut view = handle.subscribe();
    start_recording(&mut conn).await;
    harness
        .recorder
        .send(RecorderEvent::SegmentFinalized(segment(1)))
        .await
        .unwrap();
    let (seq, event) = expect_event(conn.recv().await);
    assert_eq!(seq, 1);
    assert!(matches!(event, ExamEvent::SegmentFinalized(meta) if meta.index == 1));

    drop(conn);
    let mut conn = harness.accept().await;
    assert!(matches!(conn.recv().await, ClientMessage::Hello(_)));
    conn.send(sync(ExamPhase::Running, 1)).await;
    timeout(WAIT, view.wait_for(|view| view.connected))
        .await
        .unwrap()
        .unwrap();
    let blocker = harness.block_journal_writes();

    conn.send(command(2, CommandName::EndExam)).await;
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Finalizing, RecordingState::Finalizing),
    );
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Finalizing, RecordingState::Complete),
    );
    timeout(
        WAIT,
        view.wait_for(|view| view.last_error_code.as_deref() == Some(error_code::INTERNAL_ERROR)),
    )
    .await
    .unwrap()
    .unwrap();
    std::fs::remove_dir(&blocker).unwrap();

    let (seq, event) = expect_event(conn.recv().await);
    assert_eq!(seq, 2);
    let ExamEvent::SegmentFinalized(tail) = event else {
        panic!("expected the tail segment before the summary, got {event:?}");
    };
    assert_eq!(tail, segment(2));
    let (seq, event) = expect_event(conn.recv().await);
    assert_eq!(seq, 3);
    let ExamEvent::RecordingFinalized(summary) = event else {
        panic!("expected the summary after the tail segment, got {event:?}");
    };
    assert_eq!(summary.segment_count.get(), tail.index);
    assert_eq!(
        summary.merged_sha256,
        Sha256Digest::parse(&"b".repeat(64)).unwrap()
    );
    expect_result(&conn.recv().await, 2, CommandStatus::Completed);
    conn.send(ack(2)).await;
    conn.closed().await;
}

#[tokio::test]
async fn end_with_missing_segments_replays_failure_and_clears_it_after_ack() {
    let mut harness = Harness::new().await;
    harness.script.0.lock().unwrap().omit_segment_events = true;
    let (mut conn, handle) = harness.connected(ExamPhase::Running).await;
    let mut view = handle.subscribe();
    start_recording(&mut conn).await;
    handle
        .notify(UiNotice::NavigationDenied {
            target_origin: "https://example.org".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        expect_incident(conn.recv().await, IncidentCode::NavigationDenied).0,
        1
    );

    let finalizing = state(ExecutionState::Finalizing, RecordingState::Failed);
    conn.send(command(2, CommandName::EndExam)).await;
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Finalizing, RecordingState::Finalizing),
    );
    let failure_event = conn.recv().await;
    assert_eq!(
        expect_incident(failure_event.clone(), IncidentCode::RecordingFailed).0,
        2
    );
    expect_status(&conn.recv().await, finalizing);
    let failure = |id| ClientMessage::CommandResult {
        command_id: Uuid::from_u128(id),
        status: CommandStatus::Failed,
        error_code: Some(error_code::RECORDING_FINISH_FAILED.into()),
    };
    assert_eq!(conn.recv().await, failure(2));
    drop(conn);

    let mut conn = harness.accept().await;
    let ClientMessage::Hello(hello) = conn.recv().await else {
        panic!("expected hello before replay");
    };
    assert_eq!(hello.state, finalizing);
    conn.send(sync(ExamPhase::Ended, 1)).await;
    conn.send(command(3, CommandName::EndExam)).await;
    assert_eq!(conn.recv().await, failure_event);
    assert_eq!(conn.recv().await, failure(3));
    let journal = Journal::open(&harness.candidate_dir()).await.unwrap();
    assert_eq!(journal.messages_after(0).count(), 2);

    conn.send(ack(3)).await;
    conn.closed().await;
    let finished = state(ExecutionState::Finished, RecordingState::Failed);
    timeout(
        WAIT,
        view.wait_for(|view| view.state == finished && !view.connected),
    )
    .await
    .unwrap()
    .unwrap();
    let journal = Journal::open(&harness.candidate_dir()).await.unwrap();
    assert_eq!(journal.state(), finished);
    assert_eq!(journal.messages_after(0).count(), 0);
    assert_eq!(harness.script.calls(), ["start", "finish"]);
}

#[tokio::test]
async fn capture_is_retried_after_start_exam_reports_failure() {
    let mut harness = Harness::new().await;
    let (mut conn, _handle) = harness.connected(ExamPhase::Running).await;
    harness.script.fail_next_starts(4);

    conn.send(command(1, CommandName::StartExam)).await;

    expect_incident(conn.recv().await, IncidentCode::RecordingFailed);
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Recording, RecordingState::Failed),
    );
    expect_result(&conn.recv().await, 1, CommandStatus::Failed);
    conn.send(ack(1)).await;
    // The third failure used up the attempts; retries go on and the fifth
    // start recovers.
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Recording, RecordingState::Capturing),
    );
    expect_incident(conn.recv().await, IncidentCode::RecordingRecovered);
    assert_eq!(harness.script.calls().len(), 5);
}

#[tokio::test]
async fn restart_replays_a_legacy_journal_before_replying_to_a_fresh_end_command() {
    let mut harness = Harness::new().await;
    let (mut conn, handle) = harness.connected(ExamPhase::Running).await;
    start_recording(&mut conn).await;
    conn.send(command(2, CommandName::EndExam)).await;
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Finalizing, RecordingState::Finalizing),
    );
    let (seq, segment_event) = expect_event(conn.recv().await);
    assert_eq!(seq, 1);
    assert!(matches!(&segment_event, ExamEvent::SegmentFinalized(meta) if meta.index == 1));
    let (seq, summary) = expect_event(conn.recv().await);
    assert_eq!(seq, 2);
    assert!(matches!(summary, ExamEvent::RecordingFinalized(_)));
    expect_status(
        &conn.recv().await,
        state(ExecutionState::Finalizing, RecordingState::Complete),
    );
    expect_result(&conn.recv().await, 2, CommandStatus::Completed);

    // The UI can still report a denied navigation while the end ACK is in
    // flight. No new event may trail the result and later be cleared by it.
    handle
        .notify(UiNotice::NavigationDenied {
            target_origin: "https://example.org".into(),
        })
        .await
        .unwrap();
    handle.shutdown().await.unwrap();
    conn.closed().await;

    let path = harness.candidate_dir().join("runtime/journal.json");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let event = &mut stored["queue"][1]["event"];
    assert_eq!(event["kind"], "recording_finalized");
    event["data"] = serde_json::json!({
        "segments": [segment(1)],
        "merged_sha256": "b".repeat(64),
    });
    std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

    let mut restarted = Harness::new().await;
    std::mem::swap(&mut restarted.data, &mut harness.data);
    assert!(
        Exam::has_pending_finalization(restarted.data.path(), EXAM, USER)
            .await
            .unwrap()
    );
    let pending = restarted.begin(ExamPhase::Ended).await;
    let mut conn = restarted.accept().await;
    let ClientMessage::Hello(hello) = conn.recv().await else {
        panic!("expected hello before sync");
    };
    assert_eq!(
        hello.state,
        state(ExecutionState::Finalizing, RecordingState::Complete)
    );
    // The old connection closed before receiving an end ACK. A new server
    // without the old database needs both events under their original numbers.
    conn.send(sync(ExamPhase::Ended, 0)).await;
    let handle = pending.await.unwrap().unwrap();
    conn.send(ack(2)).await;
    conn.send(command(3, CommandName::EndExam)).await;
    assert_eq!(expect_event(conn.recv().await), (1, segment_event.clone()));
    assert_eq!(expect_event(conn.recv().await), (2, summary.clone()));
    expect_result(&conn.recv().await, 3, CommandStatus::Completed);
    assert_eq!(restarted.script.calls(), Vec::<&str>::new());
    let journal = Journal::open(&restarted.candidate_dir()).await.unwrap();
    assert_eq!(
        journal
            .messages_after(0)
            .map(|(_, message)| expect_event(message))
            .collect::<Vec<_>>(),
        [(1, segment_event), (2, summary)]
    );
    conn.send(ack(3)).await;
    conn.closed().await;
    handle.shutdown().await.unwrap();
    let journal = Journal::open(&restarted.candidate_dir()).await.unwrap();
    assert_eq!(
        journal.state(),
        state(ExecutionState::Finished, RecordingState::Complete)
    );
    assert_eq!(journal.messages_after(0).count(), 0);
    assert!(
        !Exam::has_pending_finalization(restarted.data.path(), EXAM, USER)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn the_first_sync_rejects_a_fresh_runtime_if_the_exam_ended_during_admission() {
    let mut harness = Harness::new().await;
    let (entered, mut opening_recorder) = oneshot::channel();
    let (_release, wait_for_release) = oneshot::channel();
    harness.open_gate = Some(OpenGate {
        entered,
        release: wait_for_release,
    });

    let pending = harness.begin(ExamPhase::Running).await;
    let mut conn = harness.accept().await;
    assert!(matches!(conn.recv().await, ClientMessage::Hello(_)));
    conn.send(sync(ExamPhase::Ended, 0)).await;

    assert!(matches!(
        timeout(WAIT, pending).await.unwrap().unwrap(),
        Err(ExamError::ExamEnded)
    ));
    assert!(matches!(
        opening_recorder.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
    conn.closed().await;
}

#[tokio::test]
async fn finalization_requires_the_expected_account_and_ended_phase() {
    for (finalizing_user, authenticated_user, phase, expected) in [
        (Some(USER), USER + 1, ExamPhase::Ended, ExamError::Auth),
        (Some(USER), USER, ExamPhase::Running, ExamError::NotEnrolled),
        (None, USER, ExamPhase::Ended, ExamError::ExamEnded),
    ] {
        let mut harness = Harness::new().await;
        let mut journal = Journal::open(&harness.candidate_dir()).await.unwrap();
        journal.set_state(state(ExecutionState::Recording, RecordingState::Capturing));
        journal.save().await.unwrap();

        let pending = harness.launch(finalizing_user);
        harness
            .answer_enrollment(SESSION, authenticated_user, phase)
            .await;
        let result = timeout(WAIT, pending).await.unwrap().unwrap();
        assert_eq!(
            result.err().as_ref().map(std::mem::discriminant),
            Some(std::mem::discriminant(&expected))
        );
        assert!(
            Exam::has_pending_finalization(harness.data.path(), EXAM, USER)
                .await
                .unwrap()
        );
    }
}

#[tokio::test]
async fn an_ended_exam_rejects_admission_without_the_authenticated_candidates_pending_journal() {
    for (authenticated_user, local_state) in [
        (USER, None),
        (
            USER,
            Some(state(ExecutionState::Ready, RecordingState::Idle)),
        ),
        (
            USER,
            Some(state(ExecutionState::Finished, RecordingState::Failed)),
        ),
        (
            USER + 1,
            Some(state(ExecutionState::Recording, RecordingState::Capturing)),
        ),
    ] {
        let mut harness = Harness::new().await;
        let path = harness.candidate_dir().join("runtime/journal.json");
        let stored = if let Some(state) = local_state {
            let mut journal = Journal::open(&harness.candidate_dir()).await.unwrap();
            journal.set_state(state);
            journal.save().await.unwrap();
            Some(std::fs::read(&path).unwrap())
        } else {
            None
        };

        assert!(
            !Exam::has_pending_finalization(harness.data.path(), EXAM, authenticated_user)
                .await
                .unwrap()
        );
        let pending = harness.launch(Some(authenticated_user));
        harness
            .answer_enrollment(SESSION, authenticated_user, ExamPhase::Ended)
            .await;
        assert!(matches!(
            timeout(WAIT, pending).await.unwrap().unwrap(),
            Err(ExamError::ExamEnded)
        ));
        if let Some(bytes) = stored {
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        } else {
            assert!(!harness.data.path().join("exams").exists());
        }
        if authenticated_user != USER {
            assert!(
                !harness
                    .candidate_dir()
                    .with_file_name(authenticated_user.to_string())
                    .exists()
            );
        }
    }
}

#[test]
fn jittered_backoff_stays_within_half_to_full_window() {
    use super::jittered_backoff;

    let backoff = Duration::from_secs(10);
    for sample in [0, 1, 2500, 5000, u64::MAX] {
        let delay = jittered_backoff(backoff, sample);
        assert!(delay >= Duration::from_secs(5));
        assert!(delay <= backoff);
    }
    assert_eq!(jittered_backoff(Duration::ZERO, 7), Duration::ZERO);
}
