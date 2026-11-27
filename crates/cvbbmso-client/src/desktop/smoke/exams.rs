//! Drives the desktop and the real exam runtime against a loopback monitor.

mod recording;

use std::{
    fs,
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::extract::{
    Path as RoutePath, State,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use cvbbmso_protocol::{
    ClientHello, ClientMessage, CommandName, CommandStatus, ExamEvent, IncidentCode,
    RecordingPolicy, ServerMessage, error_code,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use tokio::sync::{Notify, watch};

use super::{
    AppHandle, DEADLINE, EXAM_TITLE, Emitter, Error, EventTarget, ExamPhase, HeaderMap,
    OffsetDateTime, Response, Router, SocketAddr, StatusCode, UI, Url, Uuid, WebviewWindow,
    check_ipc, evaluate, exam_list, expect_denial, get, io, mpsc, oneshot, open_fixture_browser,
    timeout, tls, wait_for,
};
use axum::response::IntoResponse;
use serde_json::{Value, json};

pub(super) const FIRST: Uuid = Uuid::from_u128(0x10);
pub(super) const SECOND: Uuid = Uuid::from_u128(0x20);
const FIXTURE_SESSION: &str = "connect.sid=synthetic-v2";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Candidate {
    Initial,
    Renewed,
    Other,
}

impl Candidate {
    fn from_headers(headers: &HeaderMap) -> Option<Self> {
        headers
            .get(axum::http::header::COOKIE)?
            .to_str()
            .ok()?
            .split("; ")
            .find_map(|pair| match pair {
                FIXTURE_SESSION => Some(Self::Initial),
                "connect.sid=renewed-candidate" => Some(Self::Renewed),
                "connect.sid=other-candidate" => Some(Self::Other),
                _ => None,
            })
    }
}

pub(super) struct Auth {
    expired: watch::Sender<bool>,
    stale_handshake: Notify,
    renewed_checks: AtomicUsize,
    other_checks: AtomicUsize,
    other_handshakes: AtomicUsize,
}

impl Auth {
    pub(super) fn new() -> Self {
        Self {
            expired: watch::Sender::new(false),
            stale_handshake: Notify::new(),
            renewed_checks: AtomicUsize::new(0),
            other_checks: AtomicUsize::new(0),
            other_handshakes: AtomicUsize::new(0),
        }
    }

    pub(super) fn enrollment(&self, headers: &HeaderMap) -> Response {
        match Candidate::from_headers(headers) {
            Some(Candidate::Initial) if *self.expired.borrow() => {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            Some(Candidate::Renewed)
                if self.renewed_checks.fetch_add(1, Ordering::Relaxed) == 0 =>
            {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            Some(Candidate::Other) => {
                self.other_checks.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        exam_list(headers, true)
    }
}

pub(in crate::desktop) fn launch() -> tauri::Result<()> {
    launch_modes(&["normal", "guard-loss", "restored-recording"])
}

pub(in crate::desktop) fn launch_recording() -> tauri::Result<()> {
    launch_modes(&["recording"])
}

fn launch_modes(modes: &[&str]) -> tauri::Result<()> {
    let directory = tempfile::tempdir()?;
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(["127.0.0.1".to_owned()]).map_err(io::Error::other)?;
    let encoded = STANDARD.encode(cert.der());
    let root = directory.path().join("root.pem");
    fs::write(
        &root,
        format!("-----BEGIN CERTIFICATE-----\n{encoded}\n-----END CERTIFICATE-----\n"),
    )?;
    fs::write(directory.path().join("leaf.der"), cert.der())?;
    fs::write(
        directory.path().join("key.der"),
        signing_key.serialize_der(),
    )?;

    // Both runtime TLS clients read native roots. Scope the fixture root to a
    // dedicated test process rather than changing the system trust store.
    for mode in modes {
        let child = Command::new(std::env::current_exe()?)
            .arg("--native-exam-smoke-child")
            .arg(directory.path())
            .arg(mode)
            .env("SSL_CERT_FILE", &root)
            .env_remove("SSL_CERT_DIR")
            .status()?;
        if !child.success() {
            return Err(io::Error::other(format!(
                "Native exam smoke ({mode}) exited with {child}"
            ))
            .into());
        }
    }
    Ok(())
}

struct Connection {
    exam_id: Uuid,
    candidate: Candidate,
    hello: ClientHello,
    outgoing: mpsc::Sender<ServerMessage>,
    results: mpsc::UnboundedReceiver<ClientMessage>,
    denials: mpsc::UnboundedReceiver<String>,
    recordings: mpsc::UnboundedReceiver<(i64, ExamEvent)>,
    closed: oneshot::Receiver<()>,
}

pub(super) struct Fixture {
    incoming: mpsc::Receiver<Connection>,
    auth: Arc<Auth>,
}

#[derive(Clone)]
struct Listener {
    connections: mpsc::Sender<Connection>,
    auth: Arc<Auth>,
}

pub(super) async fn serve(
    router: Router,
    identity: &Path,
    auth: Arc<Auth>,
) -> io::Result<(SocketAddr, CertificateDer<'static>, Fixture)> {
    let cert = CertificateDer::from(fs::read(identity.join("leaf.der"))?);
    let key = PrivatePkcs8KeyDer::from(fs::read(identity.join("key.der"))?);
    let (connections, incoming) = mpsc::channel(4);
    let router = router.merge(
        Router::new()
            .route("/api/exams/{exam_id}/ws", get(upgrade))
            .with_state(Listener {
                connections,
                auth: auth.clone(),
            }),
    );
    let (addr, _) = tls::serve_with_identity(router, cert.clone(), key.into()).await;
    Ok((addr, cert, Fixture { incoming, auth }))
}

async fn upgrade(
    RoutePath(exam_id): RoutePath<Uuid>,
    State(listener): State<Listener>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(candidate) = Candidate::from_headers(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !matches!(exam_id, FIRST | SECOND) || exam_list(&headers, true).status() != StatusCode::OK {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if candidate == Candidate::Initial && *listener.auth.expired.borrow() {
        listener.auth.stale_handshake.notify_one();
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if candidate == Candidate::Other {
        listener
            .auth
            .other_handshakes
            .fetch_add(1, Ordering::Relaxed);
    }
    ws.on_upgrade(move |ws| socket(ws, exam_id, candidate, listener))
}

async fn socket(mut ws: WebSocket, exam_id: Uuid, candidate: Candidate, listener: Listener) {
    let Ok(Some(Ok(Message::Text(frame)))) = timeout(DEADLINE, ws.recv()).await else {
        return;
    };
    let Ok(ClientMessage::Hello(hello)) = serde_json::from_str(&frame) else {
        return;
    };
    let (outgoing, mut messages) = mpsc::channel(4);
    let (report, results) = mpsc::unbounded_channel();
    let (report_denial, denials) = mpsc::unbounded_channel();
    let (report_recording, recordings) = mpsc::unbounded_channel();
    let (closed, observe_close) = oneshot::channel();
    if listener
        .connections
        .send(Connection {
            exam_id,
            candidate,
            hello,
            outgoing,
            results,
            denials,
            recordings,
            closed: observe_close,
        })
        .await
        .is_err()
    {
        return;
    }
    let mut expired = listener.auth.expired.subscribe();
    loop {
        tokio::select! {
            () = async { let _ = expired.wait_for(|expired| *expired).await; }, if candidate == Candidate::Initial => {
                let _ = ws.send(Message::Close(None)).await;
                break;
            }
            Some(message) = messages.recv() => {
                let Ok(body) = serde_json::to_string(&message) else { break };
                if ws.send(Message::Text(body.into())).await.is_err() {
                    break;
                }
            }
            frame = ws.recv() => match frame {
                Some(Ok(Message::Text(body))) => {
                    match serde_json::from_str(&body) {
                        Ok(result @ ClientMessage::CommandResult { .. }) => {
                            let _ = report.send(result);
                        }
                        Ok(ClientMessage::Event {
                            seq,
                            event: event @ (ExamEvent::SegmentFinalized(_) | ExamEvent::RecordingFinalized(_)),
                        }) => {
                            let _ = report_recording.send((seq, event));
                        }
                        Ok(ClientMessage::Event { event: ExamEvent::Incident(incident), .. })
                            if incident.code == IncidentCode::NavigationDenied =>
                        {
                            if let Some(detail) = incident.detail {
                                let _ = report_denial.send(detail);
                            }
                        }
                        _ => {}
                    }
                }
                None | Some(Err(_) | Ok(Message::Close(_))) => break,
                _ => {}
            }
        }
    }
    let _ = closed.send(());
}

async fn connection(
    connections: &mut mpsc::Receiver<Connection>,
    exam_id: Uuid,
) -> Result<Connection, Error> {
    let connection = timeout(DEADLINE, connections.recv())
        .await?
        .ok_or_else(|| io::Error::other("Monitor fixture stopped accepting connections"))?;
    assert_eq!(connection.exam_id, exam_id);
    assert!(!connection.hello.device_id.is_nil());
    assert!(!connection.hello.instance_id.is_nil());
    assert_ne!(connection.hello.device_id, connection.hello.instance_id);
    Ok(connection)
}

fn probe(ui: &WebviewWindow, expression: &str) -> tauri::Result<()> {
    ui.eval(format!(
        "window.examProbe = {{ pending: true }}; ({expression}).then(value => {{ window.examProbe = {{ value }}; }}, error => {{ window.examProbe = {{ error }}; }});"
    ))
}

async fn probe_value(ui: &WebviewWindow) -> Result<Value, Error> {
    wait_for(
        ui,
        "window.examProbe?.pending === undefined",
        "exam IPC reply",
    )
    .await?;
    evaluate(ui, "window.examProbe").await
}

fn sync() -> ServerMessage {
    let server_time = OffsetDateTime::from_unix_timestamp(1_797_729_600).unwrap();
    ServerMessage::Sync {
        server_time,
        phase: ExamPhase::Scheduled,
        start_at: server_time + time::Duration::minutes(5),
        end_at: server_time + time::Duration::hours(1),
        recording_policy: RecordingPolicy::new(30, 15).unwrap(),
        ack_event_through: 0,
    }
}

async fn open_enrolled_fixture(
    app: &AppHandle,
    ui: &WebviewWindow,
) -> Result<WebviewWindow, Error> {
    let oj = open_fixture_browser(app, ui).await?;
    check_ipc(ui, &oj).await?;
    oj.eval("location.assign('/rotate')")?;
    wait_for(
        &oj,
        "document.title === 'Fixture updated'",
        "enrolled student",
    )
    .await?;
    ui.eval(
        r"
        window.examEvents = [];
        window.__TAURI__.event.listen('cvbbmso-status', ({ payload }) => window.examEvents.push(payload))
          .then(() => document.querySelector('#refresh-exams').click());
        ",
    )?;
    wait_for(
        ui,
        "document.querySelectorAll('.open-exam:enabled').length === 2 && !document.querySelector('#exams').textContent.includes('Ended fixture exam')",
        "enrolled exams",
    )
    .await?;

    Ok(oj)
}

pub(super) async fn exercise(
    case: super::Case,
    app: &AppHandle,
    ui: &WebviewWindow,
    mut fixture: Fixture,
    foreign: SocketAddr,
    hits: &AtomicUsize,
    decisions: &mut mpsc::UnboundedReceiver<(&'static str, Url)>,
) -> Result<oneshot::Receiver<()>, Error> {
    if matches!(
        case,
        super::Case::Recording | super::Case::RestoredRecording
    ) {
        return recording::exercise(app, ui, fixture, case == super::Case::Recording).await;
    }
    let oj = open_enrolled_fixture(app, ui).await?;

    let initial_hello = check_pending_open(ui, &mut fixture.incoming).await?;
    ui.eval("document.querySelector('.open-exam').click()")?;
    let first = connection(&mut fixture.incoming, FIRST).await?;
    assert_eq!(first.hello, initial_hello);
    first.outgoing.send(sync()).await?;
    wait_for(
        ui,
        "document.querySelector('#current-exam-status').textContent.includes('已连接监考服务') && [...document.querySelectorAll('.open-exam:disabled')].map(button => button.textContent).join(',') === '已进入,进入考试'",
        "initial sync marking only the joined exam in the desktop UI",
    )
    .await?;
    assert_eq!(
        evaluate(
            ui,
            "document.querySelector('#current-exam-title').textContent"
        )
        .await?,
        EXAM_TITLE
    );
    let mut first = recover_exam(ui, &oj, first, &mut fixture, foreign, hits, decisions).await?;
    let previous = check_status_updates(ui, &mut first).await?;

    ui.eval("document.querySelector('#close-exam').click()")?;
    wait_for(ui, "document.querySelector('#current-exam').hidden && [...document.querySelectorAll('.open-exam:enabled')].every(button => button.textContent === '进入考试') && document.querySelectorAll('.open-exam:enabled').length === 2", "the closed exam UI restoring the entry buttons").await?;
    timeout(DEADLINE, first.closed).await??;
    probe(
        ui,
        "Promise.all([window.__TAURI__.core.invoke('get_exam_status'), window.__TAURI__.core.invoke('close_exam')])",
    )?;
    assert_eq!(
        probe_value(ui).await?,
        json!({"value": [null, {"closed": true}]})
    );
    app.emit_to(EventTarget::webview(UI), "cvbbmso-status", previous.clone())?;
    wait_for(ui, "window.examEvents.at(-1)?.last_error_code === 'INTERNAL_ERROR' && document.querySelector('#current-exam').hidden", "closed exam ignoring a queued status event").await?;
    assert_eq!(
        evaluate(
            ui,
            "!document.querySelector('#exams').textContent.includes('Ended fixture exam')"
        )
        .await?,
        true,
        "Refreshing session state must keep ended exams hidden"
    );

    probe(
        ui,
        &format!("window.__TAURI__.core.invoke('open_exam', {{exam_id:'{SECOND}'}})"),
    )?;
    let mut second = connection(&mut fixture.incoming, SECOND).await?;
    assert_eq!(second.hello.device_id, first.hello.device_id);
    assert_eq!(second.hello.instance_id, first.hello.instance_id);
    second.outgoing.send(sync()).await?;
    assert_eq!(
        probe_value(ui).await?,
        json!({"value": {"connected": true}})
    );
    wait_for(
        ui,
        "document.querySelector('#current-exam-title').textContent === 'Second fixture exam' && [...document.querySelectorAll('.open-exam:disabled')].map(button => button.textContent).join(',') === '进入考试,已进入'",
        "a second exam after close",
    )
    .await?;
    app.emit_to(EventTarget::webview(UI), "cvbbmso-status", previous)?;
    wait_for(ui, "window.examEvents.at(-1)?.last_error_code === 'INTERNAL_ERROR' && document.querySelector('#current-exam-title').textContent === 'Second fixture exam'", "the new exam ignoring a queued status event").await?;
    if case != super::Case::GuardLoss {
        check_status_updates(ui, &mut second).await?;
    }
    println!(
        "Exam fixture verified initial sync, enrollment failure, concurrent opens, commands, directed status, close, and reopening."
    );
    Ok(second.closed)
}

async fn recover_exam(
    ui: &WebviewWindow,
    oj: &WebviewWindow,
    first: Connection,
    fixture: &mut Fixture,
    foreign: SocketAddr,
    hits: &AtomicUsize,
    decisions: &mut mpsc::UnboundedReceiver<(&'static str, Url)>,
) -> Result<Connection, Error> {
    assert_eq!(first.candidate, Candidate::Initial);
    oj.eval("location.assign('/relogin/other')")?;
    wait_for(ui, "document.querySelector('#open').textContent === '重新登录 Accoding' && document.querySelector('#current-exam-status').textContent.includes('已连接监考服务')", "the still-connected exam rejecting another enrolled account").await?;
    assert_eq!(fixture.auth.other_checks.load(Ordering::Relaxed), 1);

    fixture.auth.expired.send_replace(true);
    timeout(DEADLINE, first.closed).await??;
    timeout(DEADLINE, fixture.auth.stale_handshake.notified()).await?;
    wait_for(ui, "document.querySelector('#current-exam-status').textContent.includes('正在重连监考服务') && document.querySelector('#open').textContent === '重新登录 Accoding' && !document.querySelector('#exam-alert').hidden && document.querySelector('#exam-alert').textContent.includes('正在自动重连')", "the expired original session reconnecting with a visible warning").await?;
    let expected_denials = block_cross_site(oj, foreign, hits, decisions).await?;
    timeout(DEADLINE, fixture.auth.stale_handshake.notified()).await?;
    assert_eq!(fixture.auth.other_checks.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.auth.other_handshakes.load(Ordering::Relaxed), 0);

    oj.eval("location.assign('/relogin/empty')")?;
    wait_for(
        oj,
        "document.title === 'Fixture session cleared'",
        "the temporarily empty native cookie store",
    )
    .await?;
    // The next retry is four seconds later, so the sampler sees the empty
    // store before the runtime attempts the original credential again.
    timeout(DEADLINE, fixture.auth.stale_handshake.notified()).await?;
    ui.eval("document.querySelector('#open').click()")?;
    wait_for(
        oj,
        "document.title === 'Fixture login' && location.pathname === '/user/login'",
        "the re-login button opening the login form",
    )
    .await?;
    oj.eval("location.assign('/relogin/same')")?;
    wait_for(ui, "window.examEvents.some(view => !view.connected && view.last_error_code === 'INTERNAL_ERROR')", "the monitor's temporary rejection of the renewed cookie").await?;
    assert_eq!(fixture.auth.renewed_checks.load(Ordering::Relaxed), 1);
    ui.eval("document.querySelector('#inspect').click()")?;
    let mut reconnected = connection(&mut fixture.incoming, FIRST).await?;
    assert_eq!(reconnected.candidate, Candidate::Renewed);
    assert_eq!(reconnected.hello, first.hello);
    reconnected.outgoing.send(sync()).await?;
    wait_for(ui, "document.querySelector('#current-exam-status').textContent.includes('已连接监考服务') && document.querySelector('#open').textContent === '打开 Accoding' && document.querySelector('#exam-alert').hidden", "the renewed original account completing sync and clearing its warning").await?;
    for expected in expected_denials {
        let detail = timeout(DEADLINE, reconnected.denials.recv()).await?;
        assert_eq!(detail.as_deref(), Some(expected.as_str()));
    }
    assert_eq!(fixture.auth.renewed_checks.load(Ordering::Relaxed), 2);
    assert_eq!(fixture.auth.other_handshakes.load(Ordering::Relaxed), 0);
    assert_eq!(evaluate(ui, "!/other-candidate|renewed-candidate/.test(document.body.innerText + JSON.stringify(window.examEvents))").await?, true);
    println!(
        "After a temporary verification failure, inspecting the same cookie recovered the original candidate and delivered only the queued origins."
    );
    Ok(reconnected)
}

async fn block_cross_site(
    oj: &WebviewWindow,
    foreign: SocketAddr,
    hits: &AtomicUsize,
    decisions: &mut mpsc::UnboundedReceiver<(&'static str, Url)>,
) -> Result<[String; 2], Error> {
    let suffix = "/fixture-private?value=private#fragment";
    let popup = Url::parse(&format!("http://localhost:{}{suffix}", foreign.port()))?;
    let navigation = Url::parse(&format!("http://{foreign}{suffix}"))?;
    oj.eval(format!(
        "window.open({}, '_blank');",
        serde_json::to_string(popup.as_str())?
    ))?;
    expect_denial(decisions, "popup", &popup).await?;
    oj.eval(format!(
        "location.assign({});",
        serde_json::to_string(navigation.as_str())?
    ))?;
    expect_denial(decisions, "navigation", &navigation).await?;
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    assert_eq!(
        evaluate(oj, "location.pathname === '/relogin/other'").await?,
        true
    );
    Ok([
        popup.origin().ascii_serialization(),
        navigation.origin().ascii_serialization(),
    ])
}

async fn check_pending_open(
    ui: &WebviewWindow,
    incoming: &mut mpsc::Receiver<Connection>,
) -> Result<ClientHello, Error> {
    ui.eval("document.querySelector('.open-exam').click()")?;
    let first_attempt = connection(incoming, FIRST).await?;
    probe(
        ui,
        &format!(
            "Promise.all([window.__TAURI__.core.invoke('get_exam_status'), ...['{FIRST}', '{SECOND}'].map(exam_id => window.__TAURI__.core.invoke('open_exam', {{exam_id}}).catch(error => error))])"
        ),
    )?;
    assert_eq!(
        probe_value(ui).await?,
        json!({"value": [null, "session_busy", "session_busy"]})
    );
    assert_eq!(
        evaluate(
            ui,
            "document.querySelector('#current-exam').hidden && window.examEvents.length === 0"
        )
        .await?,
        true
    );
    first_attempt
        .outgoing
        .send(ServerMessage::Error {
            code: error_code::NOT_ENROLLED.into(),
            message: FIXTURE_SESSION.into(),
        })
        .await?;
    wait_for(ui, "document.querySelector('#exam-session-feedback').textContent === '监考服务未核验到本场考试的报名。' && document.querySelector('#current-exam').hidden && window.examEvents.length === 0 && !document.querySelector('.open-exam').disabled", "initial rejection without a connected view").await?;
    timeout(DEADLINE, first_attempt.closed).await??;

    ui.eval("document.querySelector('.open-exam').click()")?;
    let cancelled = connection(incoming, FIRST).await?;
    probe(ui, "window.__TAURI__.core.invoke('close_exam')")?;
    assert_eq!(
        probe_value(ui).await?,
        json!({"error": "reporting_pending"})
    );
    cancelled
        .outgoing
        .send(ServerMessage::Error {
            code: error_code::NOT_ENROLLED.into(),
            message: "Fixture admission rejected".into(),
        })
        .await?;
    wait_for(
        ui,
        "!document.querySelector('.open-exam').disabled",
        "in-flight open retaining ownership until admission completes",
    )
    .await?;
    timeout(DEADLINE, cancelled.closed).await??;
    assert_eq!(evaluate(ui, "window.examEvents.length").await?, 0);
    Ok(first_attempt.hello)
}

async fn check_status_updates(ui: &WebviewWindow, first: &mut Connection) -> Result<Value, Error> {
    first
        .outgoing
        .send(ServerMessage::Error {
            code: FIXTURE_SESSION.into(),
            message: FIXTURE_SESSION.into(),
        })
        .await?;
    wait_for(ui, "window.examEvents.at(-1)?.last_error_code === 'INTERNAL_ERROR' && document.querySelector('#current-exam-status').textContent.includes('INTERNAL_ERROR')", "sanitized runtime error event").await?;
    probe(ui, "window.__TAURI__.core.invoke('get_exam_status')")?;
    let previous = probe_value(ui).await?["value"].clone();
    assert_eq!(previous["last_error_code"], error_code::INTERNAL_ERROR);
    assert_eq!(evaluate(ui, "!JSON.stringify(window.examEvents).includes('synthetic-v2') && !document.body.innerText.includes('synthetic-v2')").await?, true);

    let command_id = Uuid::from_u128(0x30);
    first
        .outgoing
        .send(ServerMessage::Command {
            command_id,
            name: CommandName::EndExam,
            effective_at: OffsetDateTime::from_unix_timestamp(1_797_729_600).unwrap(),
        })
        .await?;
    assert_eq!(
        timeout(DEADLINE, first.results.recv()).await?,
        Some(ClientMessage::CommandResult {
            command_id,
            status: CommandStatus::Failed,
            error_code: Some(error_code::RECORDING_FINISH_FAILED.into()),
        })
    );
    wait_for(ui, "window.examEvents.at(-1)?.state.recording === 'failed' && document.querySelector('#current-exam-status').textContent.includes('正在收尾') && !document.querySelector('#exam-alert').hidden && document.querySelector('#exam-alert').textContent.includes('录像收尾异常')", "server end command and visible recording failure in the desktop UI").await?;
    check_pending_close(ui).await?;
    first
        .outgoing
        .send(ServerMessage::CommandAck { command_id })
        .await?;
    wait_for(ui, "document.querySelector('#current-exam-status').textContent.includes('考试已结束') && document.querySelector('#current-exam-status').textContent.includes('录像收尾失败') && document.querySelector('#exam-session-feedback').textContent === '' && !document.querySelector('#close-exam').disabled", "final ACK updating the visible finish status and releasing the close restriction").await?;
    Ok(previous)
}

async fn check_pending_close(ui: &WebviewWindow) -> Result<(), Error> {
    ui.eval("document.querySelector('#close-exam').click()")?;
    wait_for(ui, "document.querySelector('#exam-session-feedback').textContent.includes('监考服务确认收尾后再退出') && !document.querySelector('#current-exam').hidden && !document.querySelector('#close-exam').disabled", "the exit button explaining why the exam remains open").await?;
    probe(
        ui,
        "Promise.allSettled([window.__TAURI__.core.invoke('close_exam'), window.__TAURI__.core.invoke('confirm_exit')])",
    )?;
    assert_eq!(
        probe_value(ui).await?,
        json!({"value": [
            {"status": "rejected", "reason": "reporting_pending"},
            {"status": "rejected", "reason": "reporting_pending"}
        ]})
    );
    ui.close()?;
    wait_for(ui, "document.querySelector('#exit-confirmation').open && document.querySelector('#confirm-exit').disabled && !document.querySelector('#exit-error').hidden", "unacknowledged reporting blocking window closure").await?;
    ui.eval("document.querySelector('#exit-confirmation button').click()")?;
    wait_for(
        ui,
        "!document.querySelector('#exit-confirmation').open",
        "dismissed reporting warning",
    )
    .await?;
    Ok(())
}
