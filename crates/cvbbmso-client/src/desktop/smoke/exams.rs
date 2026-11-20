//! Drives the desktop and the real exam runtime against a loopback monitor.

use std::{fs, path::Path, process::Command};

use axum::extract::{
    Path as RoutePath, State,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use cvbbmso_protocol::{
    ClientHello, ClientMessage, CommandName, CommandStatus, RecordingPolicy, ServerMessage,
    error_code,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

use super::{
    AppHandle, DEADLINE, EXAM_TITLE, Emitter, Error, EventTarget, ExamPhase, HeaderMap,
    OffsetDateTime, Response, Router, SocketAddr, StatusCode, UI, Uuid, WebviewWindow, check_ipc,
    evaluate, exam_list, get, io, mpsc, oneshot, open_fixture_browser, timeout, tls, wait_for,
};
use axum::response::IntoResponse;
use serde_json::{Value, json};

pub(super) const FIRST: Uuid = Uuid::from_u128(0x10);
pub(super) const SECOND: Uuid = Uuid::from_u128(0x20);
const FIXTURE_SESSION: &str = "connect.sid=synthetic-v2";

pub(in crate::desktop) fn launch() -> tauri::Result<()> {
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
    let child = Command::new(std::env::current_exe()?)
        .arg("--native-exam-smoke-child")
        .arg(directory.path())
        .env("SSL_CERT_FILE", root)
        .env_remove("SSL_CERT_DIR")
        .status()?;
    if child.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("Native exam smoke process exited with {child}")).into())
    }
}

pub(super) struct Connection {
    exam_id: Uuid,
    hello: ClientHello,
    outgoing: mpsc::Sender<ServerMessage>,
    results: mpsc::UnboundedReceiver<ClientMessage>,
    closed: oneshot::Receiver<()>,
}

pub(super) async fn serve(
    router: Router,
    identity: &Path,
) -> io::Result<(
    SocketAddr,
    CertificateDer<'static>,
    mpsc::Receiver<Connection>,
)> {
    let cert = CertificateDer::from(fs::read(identity.join("leaf.der"))?);
    let key = PrivatePkcs8KeyDer::from(fs::read(identity.join("key.der"))?);
    let (connections, incoming) = mpsc::channel(4);
    let router = router.merge(
        Router::new()
            .route("/api/exams/{exam_id}/ws", get(upgrade))
            .with_state(connections),
    );
    let (addr, _) = tls::serve_with_identity(router, cert.clone(), key.into()).await;
    Ok((addr, cert, incoming))
}

async fn upgrade(
    RoutePath(exam_id): RoutePath<Uuid>,
    State(connections): State<mpsc::Sender<Connection>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !matches!(exam_id, FIRST | SECOND)
        || !headers
            .get(axum::http::header::COOKIE)
            .is_some_and(|cookie| {
                cookie
                    .to_str()
                    .is_ok_and(|cookie| cookie.split("; ").any(|pair| pair == FIXTURE_SESSION))
            })
        || exam_list(&headers, true).status() != StatusCode::OK
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |ws| socket(ws, exam_id, connections))
}

async fn socket(mut ws: WebSocket, exam_id: Uuid, connections: mpsc::Sender<Connection>) {
    let Ok(Some(Ok(Message::Text(frame)))) = timeout(DEADLINE, ws.recv()).await else {
        return;
    };
    let Ok(ClientMessage::Hello(hello)) = serde_json::from_str(&frame) else {
        return;
    };
    let (outgoing, mut messages) = mpsc::channel(4);
    let (report, results) = mpsc::unbounded_channel();
    let (closed, observe_close) = oneshot::channel();
    if connections
        .send(Connection {
            exam_id,
            hello,
            outgoing,
            results,
            closed: observe_close,
        })
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            Some(message) = messages.recv() => {
                let Ok(body) = serde_json::to_string(&message) else { break };
                if ws.send(Message::Text(body.into())).await.is_err() {
                    break;
                }
            }
            frame = ws.recv() => match frame {
                Some(Ok(Message::Text(body))) => {
                    if let Ok(result @ ClientMessage::CommandResult { .. }) = serde_json::from_str(&body) {
                        let _ = report.send(result);
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

pub(super) async fn exercise(
    app: &AppHandle,
    ui: &WebviewWindow,
    mut incoming: mpsc::Receiver<Connection>,
) -> Result<oneshot::Receiver<()>, Error> {
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
        "document.querySelectorAll('.open-exam:enabled').length === 2",
        "enrolled exams",
    )
    .await?;

    let initial_hello = check_pending_open(ui, &mut incoming).await?;
    ui.eval("document.querySelector('.open-exam').click()")?;
    let mut first = connection(&mut incoming, FIRST).await?;
    assert_eq!(first.hello, initial_hello);
    first.outgoing.send(sync()).await?;
    wait_for(
        ui,
        "document.querySelector('#current-exam-status').textContent.includes('已连接监考服务')",
        "initial sync in the desktop UI",
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
    let previous = check_status_updates(ui, &mut first).await?;

    ui.eval("document.querySelector('#close-exam').click()")?;
    wait_for(ui, "document.querySelector('#current-exam').hidden && !document.querySelector('.open-exam').disabled", "the closed exam UI").await?;
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

    probe(
        ui,
        &format!("window.__TAURI__.core.invoke('open_exam', {{exam_id:'{SECOND}'}})"),
    )?;
    let second = connection(&mut incoming, SECOND).await?;
    assert_eq!(second.hello.device_id, first.hello.device_id);
    assert_eq!(second.hello.instance_id, first.hello.instance_id);
    second.outgoing.send(sync()).await?;
    assert_eq!(
        probe_value(ui).await?,
        json!({"value": {"connected": true}})
    );
    wait_for(
        ui,
        "document.querySelector('#current-exam-title').textContent === 'Second fixture exam'",
        "a second exam after close",
    )
    .await?;
    app.emit_to(EventTarget::webview(UI), "cvbbmso-status", previous)?;
    wait_for(ui, "window.examEvents.at(-1)?.last_error_code === 'INTERNAL_ERROR' && document.querySelector('#current-exam-title').textContent === 'Second fixture exam'", "the new exam ignoring a queued status event").await?;
    println!(
        "Exam fixture verified initial sync, enrollment failure, concurrent opens, commands, directed status, close, and reopening."
    );
    Ok(second.closed)
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
    wait_for(ui, "window.examProbe.pending === true && document.querySelector('#exam-session-feedback').textContent === '考试会话已关闭，请重新选择。' && document.querySelector('#current-exam').hidden", "close waiting for an in-flight open").await?;
    cancelled.outgoing.send(sync()).await?;
    assert_eq!(probe_value(ui).await?, json!({"value": {"closed": true}}));
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
    wait_for(ui, "window.examEvents.at(-1)?.state.recording === 'failed' && document.querySelector('#current-exam-status').textContent.includes('正在收尾')", "server end command in the desktop UI").await?;
    Ok(previous)
}
