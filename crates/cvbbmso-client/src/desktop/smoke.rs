//! Native smoke driver compiled into the test executable only.

use std::{
    collections::VecDeque,
    io,
    net::SocketAddr,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

pub(super) mod exams;
#[path = "../../tests/support/tls.rs"]
mod tls;

use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode, header},
    response::{AppendHeaders, Html, IntoResponse, Redirect, Response},
    routing::get,
};
use cvbbmso_protocol::{AccodingUser, ExamListResponse, ExamPhase};
use serde_json::Value;
use time::OffsetDateTime;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
    time::{sleep, timeout},
};
use uuid::Uuid;

use super::admission::fixture::Admission as FakeAdmission;
use super::*;
use crate::monitor::MAX_EXAM_LIST_BYTES;
use crate::preflight::Diagnostic;

type Error = Box<dyn std::error::Error + Send + Sync>;

const DEADLINE: Duration = Duration::from_secs(20);
const EXAM_TITLE: &str = "<b>Fixture exam</b>";
const FIXTURE_USER_ID: i64 = 42;

pub(super) struct FakeProbe(Mutex<VecDeque<Verdict>>);

impl FakeProbe {
    fn for_smoke(detected: bool) -> Self {
        let verdicts = if detected {
            VecDeque::from([Verdict::Detected(Diagnostic::GuestIdentity)])
        } else {
            VecDeque::from([
                Verdict::Unknown(Diagnostic::TimedOut),
                Verdict::NotDetected(Diagnostic::HardwareIdentity),
            ])
        };
        Self(Mutex::new(verdicts))
    }

    pub(super) fn next(&self) -> Verdict {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("Test probe exhausted")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Case {
    Desktop,
    Detected,
    Exam,
    GuardLoss,
    Recording,
    RestoredRecording,
    ExitDuringInstall,
    CleanupFailure,
}

impl Case {
    fn pauses_install(self) -> bool {
        matches!(self, Self::ExitDuringInstall | Self::CleanupFailure)
    }

    fn admission(self) -> FakeAdmission {
        let admission = FakeAdmission::default();
        match self {
            Self::Desktop => admission.require_retry(),
            case if case.pauses_install() => admission.pause(),
            _ => {}
        }
        if self == Self::CleanupFailure {
            admission.fail_shutdown();
        }
        admission
    }

    async fn check_startup_exit(
        self,
        app: &AppHandle,
        ui: &WebviewWindow,
        requests: &AtomicUsize,
        admission: &FakeAdmission,
        cleanup_checked: &AtomicBool,
    ) -> Result<bool, Error> {
        let detected = self == Self::Detected;
        check_blocked_preflight(app, ui, requests, detected).await?;
        if detected {
            ui.eval("document.querySelector('#vm-quit').click()")?;
            return Ok(true);
        }
        if !self.pauses_install() {
            return Ok(false);
        }
        check_exit_during_install(app, ui, admission).await?;
        if self == Self::CleanupFailure {
            check_cleanup_failure(app, ui, admission).await?;
            cleanup_checked.store(true, Ordering::SeqCst);
            ui.eval("document.querySelector('#admission-exit').click()")?;
        }
        Ok(true)
    }
}

pub(super) fn run(case: Case, exam_identity: Option<PathBuf>) -> tauri::Result<()> {
    let detected = case == Case::Detected;
    let data = tempfile::tempdir()?;
    let data_dir = data.path().to_owned();
    let (completed, result) = std::sync::mpsc::channel();
    let (exit_peer, exit_peer_result) = std::sync::mpsc::channel();
    let cleanup_checked = Arc::new(AtomicBool::new(false));
    let checked = cleanup_checked.clone();
    let app = builder()
        .setup(move |app| {
            check_deployment(&data_dir)?;
            let requests = Arc::new(AtomicUsize::new(0));
            let (fixture, foreign, hits) =
                async_runtime::block_on(start_fixture(requests.clone()))?;
            let (monitor, exam_connections) = if case.pauses_install() {
                (Monitor::new(None), None)
            } else {
                let (monitor, connections) =
                    start_monitor(requests.clone(), exam_identity.as_deref())?;
                (Ok(monitor), connections)
            };
            let base = Url::parse(&format!("http://{fixture}/"))?;
            let (blocked, mut decisions) = mpsc::unbounded_channel();
            app.manage(Desktop {
                login: base.join("user/login")?,
                identity: base.join("api/users/me")?,
                data_dir,
                monitor,
                blocked: Some(blocked),
            });
            let admission = case.admission();
            app.manage(admission.clone());
            app.manage(FakeProbe::for_smoke(detected));
            let ui = build_ui(app.handle())?;
            let app = app.handle().clone();
            async_runtime::spawn(async move {
                let exercised: Result<(), Error> = async {
                    if case
                        .check_startup_exit(&app, &ui, &requests, &admission, &checked)
                        .await?
                    {
                        return Ok(());
                    }
                    if let Some(connections) = exam_connections {
                        let closed = exams::exercise(
                            case,
                            &app,
                            &ui,
                            connections,
                            foreign,
                            &hits,
                            &mut decisions,
                        )
                        .await?;
                        assert_eq!(admission.installs(), 1);
                        assert_eq!(admission.shutdowns(), 0);
                        if case == Case::GuardLoss {
                            check_guard_loss(&ui, &admission, closed).await?;
                        } else {
                            exit_peer.send(closed)?;
                        }
                    } else {
                        check_admission_retry(&ui, &admission).await?;
                        exercise(&app, &ui, &base, foreign, &hits, &mut decisions).await?;
                    }
                    ui.close().map_err(Into::into)
                }
                .await;
                if let Err(error) = exercised {
                    eprintln!("Desktop smoke failed: {error}");
                } else {
                    sleep(DEADLINE).await;
                    eprintln!("Desktop smoke failed: closing the control window did not exit");
                }
                checked.store(false, Ordering::SeqCst);
                app.exit(1);
            });
            Ok(())
        })
        .build(tauri::generate_context!())?;
    let expected_code = i32::from(case == Case::CleanupFailure);
    let handle = app.handle().clone();
    let runtime_code = app.run_return(move |app, event| {
        exam::handle_exit(app, &event);
        if exit_matches_case(app, &event, case, &cleanup_checked) {
            let _ = completed.send(());
        }
    });
    if exam::exit_code(&handle, runtime_code) != expected_code {
        return Err(io::Error::other("Unexpected desktop smoke exit code").into());
    }
    result
        .try_recv()
        .map_err(|_| io::Error::other("Native desktop smoke did not complete successfully"))?;
    if matches!(case, Case::Exam | Case::Recording | Case::RestoredRecording) {
        check_exit_peer(&exit_peer_result)?;
    }
    println!("Desktop smoke passed.");
    Ok(())
}

fn exit_matches_case(
    app: &AppHandle,
    event: &tauri::RunEvent,
    case: Case,
    cleanup_checked: &AtomicBool,
) -> bool {
    let failure = case == Case::CleanupFailure;
    let expect_browser = case != Case::Detected && !case.pauses_install();
    matches!(event, tauri::RunEvent::ExitRequested { code: Some(code), .. } if *code == i32::from(failure))
        && exam::exit_drained(app)
        && app.get_webview_window(OJ).is_some() == expect_browser
        && app.state::<FakeAdmission>().shutdowns() == usize::from(case != Case::Detected)
        && (!failure || cleanup_checked.load(Ordering::SeqCst))
}

fn check_exit_peer(peer: &std::sync::mpsc::Receiver<oneshot::Receiver<()>>) -> io::Result<()> {
    let peer = peer.try_recv().map_err(io::Error::other)?;
    async_runtime::block_on(async { timeout(DEADLINE, peer).await })
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?;
    println!("Exiting with an active exam drained the runtime and closed its monitor socket.");
    Ok(())
}

async fn check_admission_retry(ui: &WebviewWindow, admission: &FakeAdmission) -> Result<(), Error> {
    wait_for(ui, "!document.querySelector('#admission-retry').hidden && document.querySelector('#admission-status').textContent.includes('代理')", "proxy preflight denial").await?;
    assert_eq!(admission.installs(), 0);
    check_environment_gate(ui).await?;
    ui.eval("document.querySelector('#admission-retry').click()")?;
    #[cfg(target_os = "macos")]
    {
        wait_for(ui, "!document.querySelector('#admission-retry').hidden && document.querySelector('#admission-status').textContent.includes('录制屏幕')", "screen permission denial").await?;
        assert_eq!(admission.installs(), 0);
        check_environment_gate(ui).await?;
        ui.eval("document.querySelector('#admission-retry').click()")?;
        check_helper_approval(ui, admission).await?;
    }
    timeout(DEADLINE, admission.install_started()).await?;
    ui.eval(r"
        window.admissionProbe = 'pending';
        const invoke = window.__TAURI__.core.invoke;
        Promise.all(Array.from({length: 4}, () => invoke('admission_status', {retry: true})))
          .then(results => { window.admissionProbe = results.every(result => result.status === 'checking'); });
    ")?;
    wait_for(
        ui,
        "window.admissionProbe === true && document.querySelector('#admitted-controls').hidden",
        "concurrent retries sharing one installation",
    )
    .await?;
    check_environment_gate(ui).await?;
    assert_eq!(admission.installs(), 1);
    admission.resume();

    wait_for(
        ui,
        "!document.querySelector('#admitted-controls').hidden",
        "admission after retry",
    )
    .await?;
    assert_eq!(admission.installs(), 1);
    assert_eq!(admission.shutdowns(), 0);
    println!("Startup checks preceded installation; concurrent retries installed one guard.");
    Ok(())
}

#[cfg(target_os = "macos")]
async fn check_helper_approval(ui: &WebviewWindow, fake: &FakeAdmission) -> Result<(), Error> {
    wait_for(
        ui,
        "!document.querySelector('#admission-settings').hidden && document.querySelector('#admission-status').textContent.includes('登录项与扩展')",
        "the native helper approval prompt",
    )
    .await?;
    check_environment_gate(ui).await?;
    assert_eq!(fake.installs(), 0);
    assert_eq!(fake.settings_opens(), 0);

    ui.eval("document.querySelector('#admission-settings').click()")?;
    timeout(DEADLINE, fake.settings_opened()).await?;
    assert_eq!(fake.settings_opens(), 1);
    check_environment_gate(ui).await?;
    assert_eq!(fake.installs(), 0);

    fake.grant_approval();
    let app = ui.app_handle();
    app.state::<admission::Admission>().recheck_approval(app);
    wait_for(
        ui,
        "document.querySelector('#admission-settings').hidden && !document.querySelector('#admission-retry').hidden && document.querySelector('#admission-status').textContent.includes('PF 配置')",
        "PF configuration blocking the approved helper",
    )
    .await?;
    check_environment_gate(ui).await?;
    assert_eq!(fake.installs(), 0);

    fake.enable_pf();
    ui.eval("document.querySelector('#admission-retry').click()")?;
    println!(
        "A pending approval and unavailable PF both blocked admission; only the local settings action opened settings."
    );
    Ok(())
}

async fn check_environment_gate(ui: &WebviewWindow) -> Result<(), Error> {
    ui.eval(
        r"
        window.gatedRequests = 'pending';
        (async () => {
            const invoke = window.__TAURI__.core.invoke;
            const results = await Promise.allSettled([
                invoke('open_accoding'), invoke('inspect_session'), invoke('list_exams'),
                invoke('open_exam', {exam_id: '00000000-0000-0000-0000-000000000010'})
            ]);
            window.gatedRequests = results.every(result => result.status === 'rejected')
                && results[2].reason === 'preflight_blocked'
                && results[3].reason === 'admission_unavailable';
        })();
    ",
    )?;
    wait_for(
        ui,
        "window.gatedRequests === true",
        "Rust rejecting direct IPC while admission is blocked",
    )
    .await
}

async fn check_exit_during_install(
    app: &AppHandle,
    ui: &WebviewWindow,
    admission: &FakeAdmission,
) -> Result<(), Error> {
    timeout(DEADLINE, admission.install_started()).await?;
    ui.eval(
        r"
        window.missingMonitor = 'pending';
        window.__TAURI__.core.invoke('list_exams').then(
            () => { window.missingMonitor = 'unexpected_success'; },
            error => { window.missingMonitor = error; },
        );
    ",
    )?;
    wait_for(
        ui,
        "window.missingMonitor === 'monitor_unconfigured'",
        "list_exams reporting the missing monitor after VM preflight",
    )
    .await?;
    ui.close()?;
    wait_for(
        ui,
        "document.querySelector('#admission-status').textContent.includes('关闭客户端')",
        "exit waiting for the in-flight guard",
    )
    .await?;
    assert!(!exam::exit_drained(app));
    assert_eq!(admission.shutdowns(), 0);
    ui.close()?;
    wait_for(
        ui,
        "document.querySelector('#admission-status').textContent.includes('关闭客户端')",
        "a repeated close leaving the in-flight cleanup in progress",
    )
    .await?;
    admission.resume();
    println!("Exit deferred until the in-flight install returned its guard.");
    Ok(())
}

async fn check_cleanup_failure(
    app: &AppHandle,
    ui: &WebviewWindow,
    admission: &FakeAdmission,
) -> Result<(), Error> {
    wait_for(
        ui,
        "document.querySelector('#admission-status').textContent.includes('permission_denied') && !document.querySelector('#admission-exit').hidden && document.querySelector('#admitted-controls').hidden && document.querySelector('#admission-retry').hidden",
        "cleanup diagnostics and explicit exit",
    )
    .await?;
    assert!(!exam::exit_drained(app));
    assert_eq!(admission.shutdowns(), 1);

    let diagnostic = app
        .state::<Desktop>()
        .data_dir
        .join(admission::CLEANUP_DIAGNOSTIC);
    let saved: Value = serde_json::from_slice(&std::fs::read(diagnostic)?)?;
    assert_eq!(saved["code"], "permission_denied");
    assert_eq!(saved["operation"], "egress_shutdown");
    assert_eq!(saved["platform"], std::env::consts::OS);
    assert!(saved["unix_seconds"].is_u64());
    assert_eq!(saved.as_object().map(serde_json::Map::len), Some(4));

    ui.eval(
        r"
        window.cleanupRetryBlocked = false;
        window.__TAURI__.core.invoke('admission_status', {retry: true}).then(({status, reason}) => {
            window.cleanupRetryBlocked = status === 'cleanup_failed'
                && reason.diagnostic_saved === true && reason.code === 'permission_denied';
        });
    ",
    )?;
    wait_for(ui, "window.cleanupRetryBlocked", "terminal cleanup failure").await?;
    assert_eq!(admission.installs(), 1);
    assert_eq!(admission.shutdowns(), 1);
    println!("Cleanup failure retained its category on screen and in the local diagnostic.");
    Ok(())
}

async fn check_guard_loss(
    ui: &WebviewWindow,
    admission: &FakeAdmission,
    peer: oneshot::Receiver<()>,
) -> Result<(), Error> {
    assert!(admission.verifies() > 0);
    admission.lose_guard();
    wait_for(ui, "document.querySelector('#admission-status').textContent.includes('联系监考人员') && document.querySelector('#admitted-controls').hidden", "terminal guard loss").await?;
    check_environment_gate(ui).await?;
    timeout(DEADLINE, peer).await??;
    ui.eval(r"
        window.guardLost = false;
        Promise.all([
            window.__TAURI__.core.invoke('get_exam_status'),
            window.__TAURI__.core.invoke('admission_status', {retry: true})
        ]).then(([exam, admission]) => {
            window.guardLost = exam === null && admission.status === 'blocked' && admission.reason === 'guard_lost';
        });
    ")?;
    wait_for(
        ui,
        "window.guardLost",
        "guard loss draining the exam and refusing retry",
    )
    .await?;
    assert_eq!(admission.installs(), 1);
    assert_eq!(admission.shutdowns(), 0);
    println!(
        "Guard failure blocked direct IPC, drained the active exam, and retained the guard until app exit."
    );
    Ok(())
}

fn check_deployment(data_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let deployed = Desktop::deployed(data_dir.to_path_buf(), None)?;
    assert!(matches!(deployed.monitor, Err(MonitorError::Configuration)));
    for invalid in ["", "http://monitor.example.org"] {
        assert!(
            Desktop::deployed(data_dir.to_path_buf(), Some(invalid)).is_err_and(|error| error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::InvalidInput))
        );
    }
    println!("Missing monitor configuration is allowed; invalid origins fail startup.");
    Ok(())
}

fn start_monitor(
    requests: Arc<AtomicUsize>,
    exam_identity: Option<&Path>,
) -> io::Result<(Monitor, Option<exams::Fixture>)> {
    let exam_smoke = exam_identity.is_some();
    let auth = Arc::new(exams::Auth::new());
    let http_auth = auth.clone();
    let router = Router::new().route(
        "/api/exams",
        get(move |headers: HeaderMap| {
            let requests = requests.clone();
            let auth = http_auth.clone();
            async move {
                requests.fetch_add(1, Ordering::Relaxed);
                if exam_smoke {
                    auth.enrollment(&headers)
                } else {
                    exam_list(&headers, false)
                }
            }
        }),
    );
    let (addr, cert, connections) = if let Some(identity) = exam_identity {
        let (addr, cert, connections) =
            async_runtime::block_on(exams::serve(router, identity, auth))?;
        (addr, cert, Some(connections))
    } else {
        let (addr, cert, _) = async_runtime::block_on(tls::serve(router));
        (addr, cert, None)
    };
    let origin = Url::parse(&format!("https://{addr}"))
        .map_err(io::Error::other)?
        .origin();
    let cert = reqwest::Certificate::from_der(cert.as_ref()).map_err(io::Error::other)?;
    let monitor = Monitor::with_root_certificate(origin, cert).map_err(io::Error::other)?;
    Ok((monitor, connections))
}

async fn start_fixture(
    requests: Arc<AtomicUsize>,
) -> io::Result<(SocketAddr, SocketAddr, Arc<AtomicUsize>)> {
    let fixture = TcpListener::bind(("127.0.0.1", 0)).await?;
    let foreign = TcpListener::bind(("127.0.0.1", 0)).await?;
    let fixture_addr = fixture.local_addr()?;
    let foreign_addr = foreign.local_addr()?;
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        let router = Router::new().fallback(move || async move {
            counter.fetch_add(1, Ordering::Relaxed);
            Html("<title>Unexpected origin</title>")
        });
        axum::serve(foreign, router).await.unwrap();
    });
    tokio::spawn(async move {
        let router = Router::new()
            .route(
                "/user/login",
                get(move || {
                    requests.fetch_add(1, Ordering::Relaxed);
                    async {
                        (
                            AppendHeaders([
                                (
                                    header::SET_COOKIE,
                                    "connect.sid=synthetic-v1; Path=/; HttpOnly",
                                ),
                                (header::SET_COOKIE, "_aux=synthetic; Path=/api; HttpOnly"),
                                (
                                    header::SET_COOKIE,
                                    "_ignored=synthetic; Path=/user; HttpOnly",
                                ),
                            ]),
                            Html("<title>Fixture login</title>"),
                        )
                    }
                }),
            )
            .route(
                "/rotate",
                get(|| async {
                    (
                        [(
                            header::SET_COOKIE,
                            "connect.sid=synthetic-v2; Path=/; HttpOnly",
                        )],
                        Html("<title>Fixture updated</title>"),
                    )
                }),
            )
            .route(
                "/relogin/{mode}",
                get(
                    |axum::extract::Path(mode): axum::extract::Path<String>| async move {
                        fixture_relogin(&mode)
                    },
                ),
            )
            .route(
                "/expired",
                get(|| async {
                    (
                        [(header::SET_COOKIE, "connect.sid=expired; Path=/; HttpOnly")],
                        Html("<title>Expired session</title>"),
                    )
                }),
            )
            .route(
                "/unavailable",
                get(|| async {
                    (
                        [(
                            header::SET_COOKIE,
                            "connect.sid=unavailable; Path=/; HttpOnly",
                        )],
                        Html("<title>Unavailable service</title>"),
                    )
                }),
            )
            .route(
                "/oversized",
                get(|| async {
                    (
                        [(
                            header::SET_COOKIE,
                            "connect.sid=oversized; Path=/; HttpOnly",
                        )],
                        Html("<title>Oversized list</title>"),
                    )
                }),
            )
            .route(
                "/redirect",
                get(move || async move { Redirect::temporary(&format!("http://{foreign_addr}/")) }),
            );
        axum::serve(fixture, router).await.unwrap();
    });
    Ok((fixture_addr, foreign_addr, hits))
}

fn fixture_relogin(mode: &str) -> Response {
    let (cookie, page) = match mode {
        "other" => (
            "connect.sid=other-candidate; Path=/; HttpOnly",
            "<title>Fixture other candidate</title>",
        ),
        "same" => (
            "connect.sid=renewed-candidate; Path=/; HttpOnly",
            "<title>Fixture renewed candidate</title>",
        ),
        "empty" => (
            "connect.sid=; Path=/; Max-Age=0; HttpOnly",
            "<title>Fixture session cleared</title>",
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::SET_COOKIE, cookie)], Html(page)).into_response()
}

fn exam_list(headers: &HeaderMap, include_second_exam: bool) -> Response {
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let mut parts = cookie.split("; ").collect::<Vec<_>>();
    parts.sort_unstable();
    let (user_id, enrolled) = match parts.as_slice() {
        ["_aux=synthetic", "connect.sid=synthetic-v1"] => (FIXTURE_USER_ID, false),
        ["_aux=synthetic", "connect.sid=synthetic-v2"] => (FIXTURE_USER_ID, true),
        ["_aux=synthetic", "connect.sid=renewed-candidate"] if include_second_exam => {
            (FIXTURE_USER_ID, true)
        }
        ["_aux=synthetic", "connect.sid=other-candidate"] if include_second_exam => (43, true),
        ["_aux=synthetic", "connect.sid=unavailable"] => {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        ["_aux=synthetic", "connect.sid=oversized"] => {
            let mut body =
                br#"{"user":{"id":42,"username":"test","nickname":"Test"},"items":[]}"#.to_vec();
            body.resize(MAX_EXAM_LIST_BYTES + 1, b' ');
            return ([(header::CONTENT_TYPE, "application/json")], body).into_response();
        }
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let start_at = OffsetDateTime::from_unix_timestamp(1_797_729_600).unwrap();
    let end_at = OffsetDateTime::from_unix_timestamp(1_797_733_200).unwrap();
    let summary = |id, title: &str| {
        ExamSummary::new(id, title.into(), start_at, end_at, ExamPhase::Scheduled).unwrap()
    };
    let mut items = vec![];
    if enrolled {
        items.push(summary(exams::FIRST, EXAM_TITLE));
        if include_second_exam {
            items.push(summary(exams::SECOND, "Second fixture exam"));
        }
    }
    Json(ExamListResponse {
        user: AccodingUser::new(
            user_id,
            format!("test-student-{user_id}"),
            "Test Student".into(),
        )
        .unwrap(),
        items,
    })
    .into_response()
}

async fn evaluate(window: &WebviewWindow, script: &str) -> Result<Value, Error> {
    let (send, receive) = oneshot::channel();
    let send = std::sync::Mutex::new(Some(send));
    window.eval_with_callback(script, move |output| {
        if let Ok(mut slot) = send.lock()
            && let Some(send) = slot.take()
        {
            let _ = send.send(output);
        }
    })?;
    let value = timeout(DEADLINE, receive).await??;
    Ok(serde_json::from_str(&value)?)
}

async fn wait_for(window: &WebviewWindow, script: &str, what: &str) -> Result<(), Error> {
    timeout(DEADLINE, async {
        loop {
            if evaluate(window, script)
                .await
                .is_ok_and(|value| value == true)
            {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| io::Error::other(format!("Timed out waiting for {what}")))?;
    Ok(())
}

async fn check_blocked_preflight(
    app: &AppHandle,
    ui: &WebviewWindow,
    requests: &AtomicUsize,
    detected: bool,
) -> Result<(), Error> {
    let (message, description) = if detected {
        (
            "检测到虚拟机。请在实体设备上运行客户端。",
            "VM blocking screen",
        )
    } else {
        ("无法完成设备检查，请重试。", "retryable preflight screen")
    };
    let text = serde_json::to_string(message)?;
    wait_for(ui, &format!(
        "document.querySelector('#vm-status').textContent === {text} && document.querySelector('#student-controls').hidden && document.querySelector('#vm-retry').hidden === {detected} && !document.querySelector('#vm-quit').hidden"
    ), description).await?;
    ui.eval(r"
        window.blockedCommands = 'pending';
        (async () => {
          const { invoke } = window.__TAURI__.core;
          const results = await Promise.allSettled([
            invoke('open_accoding'),
            invoke('inspect_session'),
            invoke('list_exams'),
            invoke('open_exam', { exam_id: '00000000-0000-0000-0000-000000000010' }),
          ]);
          const denied = ['设备检查通过后才能继续。', '设备检查通过后才能继续。', 'preflight_blocked', 'preflight_blocked'];
          window.blockedCommands = results.every(
            (result, index) => result.status === 'rejected' && result.reason === denied[index]
          ) ? 'rejected' : 'unexpected';
        })();
    ")?;
    wait_for(
        ui,
        "window.blockedCommands === 'rejected'",
        "Rust admission rejection",
    )
    .await?;
    if app.get_webview_window(OJ).is_some() || requests.load(Ordering::Relaxed) != 0 {
        return Err(
            io::Error::other("Blocked client created a remote WebView or made a request").into(),
        );
    }
    assert_eq!(app.state::<FakeAdmission>().installs(), 0);
    println!("{description} rejected direct IPC without any remote WebView or request.");
    if detected {
        ui.eval(
            r"
            window.vmRetryResult = 'pending';
            window.__TAURI__.core.invoke('vm_preflight', { retry: true })
              .then((result) => { window.vmRetryResult = result.status; });
        ",
        )?;
        wait_for(
            ui,
            "window.vmRetryResult === 'detected'",
            "terminal VM verdict",
        )
        .await?;
    } else {
        ui.eval("document.querySelector('#vm-retry').click()")?;
        wait_for(
            ui,
            "document.querySelector('#vm-preflight').hidden && !document.querySelector('#student-controls').hidden",
            "student controls after a successful retry",
        ).await?;
        println!("Preflight retry admitted the client and revealed student controls.");
    }
    Ok(())
}

async fn expect_cookie(oj: &WebviewWindow, base: &Url, session: &str) -> Result<(), Error> {
    let oj = oj.clone();
    let identity = base.join("api/users/me")?;
    let observed = async_runtime::spawn_blocking(move || read_cookies(&oj, identity))
        .await?
        .map_err(|_| io::Error::other("The native store did not return a valid session"))?;
    let header = observed.header_value();
    let parts: Vec<_> = header.to_str()?.split("; ").collect();
    if parts.len() != 2 || !parts.contains(&session) || !parts.contains(&"_aux=synthetic") {
        return Err(
            io::Error::other("Cookie scope, value or update did not match the fixture").into(),
        );
    }
    Ok(())
}

async fn exercise(
    app: &AppHandle,
    ui: &WebviewWindow,
    base: &Url,
    foreign: SocketAddr,
    hits: &AtomicUsize,
    decisions: &mut mpsc::UnboundedReceiver<(&'static str, Url)>,
) -> Result<(), Error> {
    let oj = open_fixture_browser(app, ui).await?;
    println!(
        "Native WebView user agent: {}",
        evaluate(&oj, "navigator.userAgent").await?
    );
    platform::assert_locked(&oj)?;
    expect_cookie(&oj, base, "connect.sid=synthetic-v1").await?;
    if evaluate(&oj, "document.cookie.includes('connect.sid')").await? != false {
        return Err(io::Error::other("HttpOnly cookie was visible to page scripts").into());
    }

    check_ipc(ui, &oj).await?;
    refresh_exams(ui, "已登录，暂无报名考试。", None).await?;
    let foreign_url = Url::parse(&format!("http://{foreign}/"))?;
    oj.eval(format!(
        "window.open('{}', '_blank');",
        foreign_url.as_str()
    ))?;
    expect_denial(decisions, "popup", &foreign_url).await?;
    oj.eval("window.open('/rotate', '_blank');")?;
    wait_for(
        &oj,
        "document.title === 'Fixture updated'",
        "same-origin popup routed in place",
    )
    .await?;
    check_reopen(ui, &oj).await?;
    expect_cookie(&oj, base, "connect.sid=synthetic-v2").await?;
    check_exam_refresh(ui, &oj, base).await?;
    oj.eval("location.assign('/redirect')")?;
    expect_denial(decisions, "navigation", &foreign_url).await?;
    if hits.load(Ordering::Relaxed) != 0 {
        return Err(io::Error::other("Cross-origin navigation reached the listener").into());
    }
    Ok(())
}

async fn open_fixture_browser(app: &AppHandle, ui: &WebviewWindow) -> Result<WebviewWindow, Error> {
    wait_for(
        ui,
        "document.querySelector('#inspect')?.disabled === false && !document.querySelector('#admitted-controls').hidden",
        "local UI event subscription",
    )
    .await?;
    ui.eval("document.querySelector('#open').click()")?;
    let oj = timeout(DEADLINE, async {
        loop {
            if let Some(oj) = app.get_webview_window(OJ) {
                return oj;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| io::Error::other("Local command did not open the Accoding window"))?;
    wait_for(&oj, "document.title === 'Fixture login'", "remote fixture").await?;
    Ok(oj)
}

async fn refresh_exams(ui: &WebviewWindow, status: &str, exam: Option<&str>) -> Result<(), Error> {
    wait_for(
        ui,
        "document.querySelector('#refresh-exams')?.disabled === false",
        "exam refresh button",
    )
    .await?;
    ui.eval("document.querySelector('#refresh-exams').click()")?;
    let text = serde_json::to_string(status)?;
    let title = serde_json::to_string(&exam)?;
    let row_count = usize::from(exam.is_some());
    let row_check = if exam.is_some() {
        format!(" && document.querySelector('#exams h3').textContent === {title}")
    } else {
        String::new()
    };
    let assertion = format!(
        "document.querySelector('#exam-status').textContent === {text} &&          document.querySelector('#exams').childElementCount === {row_count}{row_check}"
    );
    wait_for(ui, &assertion, status).await
}

async fn check_exam_refresh(
    ui: &WebviewWindow,
    oj: &WebviewWindow,
    base: &Url,
) -> Result<(), Error> {
    let enrolled = "已加载 1 场考试。";
    refresh_exams(ui, enrolled, Some(EXAM_TITLE)).await?;
    for (route, status) in [
        ("expired", "请在 Accoding 窗口登录或重新登录，再刷新列表。"),
        ("unavailable", "监考服务暂时无法核验身份，请稍后重试。"),
        ("oversized", "监考服务返回了异常响应，请稍后重试。"),
    ] {
        oj.navigate(base.join(route)?)?;
        let path = serde_json::to_string(&format!("/{route}"))?;
        let ready = format!("location.pathname === {path} && document.readyState === 'complete'");
        wait_for(oj, &ready, route).await?;
        refresh_exams(ui, status, None).await?;

        oj.navigate(base.join("rotate")?)?;
        wait_for(
            oj,
            "document.title === 'Fixture updated'",
            "restored enrollment session",
        )
        .await?;
        refresh_exams(ui, enrolled, Some(EXAM_TITLE)).await?;
    }
    println!(
        "Exam list loaded and refreshed through HTTPS; empty, expired, unavailable, and oversized responses verified."
    );
    Ok(())
}

async fn check_reopen(ui: &WebviewWindow, oj: &WebviewWindow) -> Result<(), Error> {
    oj.close()?;
    wait_for_visibility(oj, false).await?;
    ui.eval("document.querySelector('#open').click()")?;
    wait_for_visibility(oj, true).await?;
    wait_for(
        oj,
        "document.title === 'Fixture updated'",
        "the original Accoding page",
    )
    .await
}

async fn wait_for_visibility(window: &WebviewWindow, visible: bool) -> Result<(), Error> {
    timeout(DEADLINE, async {
        while window.is_visible()? != visible {
            sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, tauri::Error>(())
    })
    .await
    .map_err(|_| io::Error::other(format!("Accoding visibility did not change to {visible}")))??;
    Ok(())
}

async fn expect_denial(
    decisions: &mut mpsc::UnboundedReceiver<(&'static str, Url)>,
    action: &str,
    target: &Url,
) -> Result<(), Error> {
    timeout(DEADLINE, async {
        while let Some((observed_action, observed_target)) = decisions.recv().await {
            if observed_action == action && &observed_target == target {
                return Ok(());
            }
        }
        Err(io::Error::other(
            "Browser closed before reporting its navigation decision",
        ))
    })
    .await
    .map_err(|_| io::Error::other(format!("WebView did not reject the cross-origin {action}")))??;
    Ok(())
}

async fn check_ipc(ui: &WebviewWindow, oj: &WebviewWindow) -> Result<(), Error> {
    oj.eval(
        r#"
        window.boundaryResult = "pending";
        (async () => {
            const invoke = window.__TAURI_INTERNALS__?.invoke;
            if (!invoke) {
                window.boundaryResult = "blocked";
                return;
            }
            const results = await Promise.allSettled([
                invoke("vm_preflight", { retry: true }),
                invoke("admission_status", { retry: true }),
                invoke("open_helper_approval_settings"),
                invoke("inspect_session"),
                invoke("list_exams"),
                invoke("open_exam", { exam_id: "00000000-0000-0000-0000-000000000010" }),
                invoke("get_exam_status"),
                invoke("close_exam"),
                invoke("plugin:event|listen", {
                    event: "accoding-session",
                    target: { kind: "Any" },
                    handler: window.__TAURI_INTERNALS__.transformCallback(() => {})
                }),
                invoke("plugin:event|listen", {
                    event: "cvbbmso-status",
                    target: { kind: "Webview", label: "student" },
                    handler: window.__TAURI_INTERNALS__.transformCallback(() => {})
                }),
            ]);
            window.boundaryResult = results.every(({status}) => status === "rejected")
                ? "blocked" : "exposed";
        })();
    "#,
    )?;
    wait_for(
        oj,
        "window.boundaryResult === 'blocked'",
        "remote IPC denial",
    )
    .await?;

    ui.eval("document.querySelector('#inspect').click()")?;
    wait_for(
        ui,
        "document.querySelector('#session').textContent === '已读取 Accoding 会话。'",
        "local session event",
    )
    .await?;
    Ok(())
}
