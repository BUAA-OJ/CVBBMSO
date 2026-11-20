//! Native smoke driver compiled into the test executable only.

use std::{
    collections::VecDeque,
    io,
    net::SocketAddr,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
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

use super::*;
use crate::monitor::MAX_EXAM_LIST_BYTES;
use crate::preflight::Diagnostic;

type Error = Box<dyn std::error::Error + Send + Sync>;

const DEADLINE: Duration = Duration::from_secs(20);
const EXAM_TITLE: &str = "<b>Fixture exam</b>";

pub(super) struct FakeProbe(Mutex<VecDeque<Verdict>>);

impl FakeProbe {
    pub(super) fn next(&self) -> Verdict {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("Test probe exhausted")
    }
}

pub(super) fn run(detected: bool, exam_identity: Option<PathBuf>) -> tauri::Result<()> {
    let data = tempfile::tempdir()?;
    let data_dir = data.path().to_owned();
    let (completed, result) = std::sync::mpsc::channel();
    let (exit_peer, exit_peer_result) = std::sync::mpsc::channel();
    let exam_smoke = exam_identity.is_some();
    let app = builder()
        .setup(move |app| {
            let deployed = Desktop::deployed(data_dir.clone(), None)?;
            assert!(matches!(deployed.monitor, Err(MonitorError::Configuration)));
            for invalid in ["", "http://monitor.example.org"] {
                assert!(
                    Desktop::deployed(data_dir.clone(), Some(invalid)).is_err_and(|error| error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|error| error.kind() == io::ErrorKind::InvalidInput))
                );
            }
            println!("Missing monitor configuration is allowed; invalid origins fail startup.");
            let requests = Arc::new(AtomicUsize::new(0));
            let (fixture, foreign, hits) =
                async_runtime::block_on(start_fixture(requests.clone()))?;
            let (monitor, exam_connections) =
                start_monitor(requests.clone(), exam_identity.as_deref())?;
            let base = Url::parse(&format!("http://{fixture}/"))?;
            let (blocked, mut decisions) = mpsc::unbounded_channel();
            app.manage(Desktop {
                login: base.join("user/login")?,
                identity: base.join("api/users/me")?,
                data_dir,
                monitor: Ok(monitor),
                blocked: Some(blocked),
                fixture_exam_admission: exam_smoke,
            });
            let outcomes = if detected {
                VecDeque::from([Verdict::Detected(Diagnostic::GuestIdentity)])
            } else {
                VecDeque::from([
                    Verdict::Unknown(Diagnostic::TimedOut),
                    Verdict::NotDetected(Diagnostic::HardwareIdentity),
                ])
            };
            app.manage(FakeProbe(Mutex::new(outcomes)));
            let ui = build_ui(app.handle())?;
            let app = app.handle().clone();
            async_runtime::spawn(async move {
                let exercised: Result<(), Error> = async {
                    check_blocked_preflight(&app, &ui, &requests, detected).await?;
                    if detected {
                        return ui
                            .eval("document.querySelector('#vm-quit').click()")
                            .map_err(Into::into);
                    }
                    if let Some(connections) = exam_connections {
                        let closed = exams::exercise(&app, &ui, connections).await?;
                        exit_peer.send(closed)?;
                    } else {
                        exercise(&app, &ui, &deployed, &base, foreign, &hits, &mut decisions)
                            .await?;
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
                app.exit(1);
            });
            Ok(())
        })
        .build(tauri::generate_context!())?;
    app.run_return(move |app, event| {
        exam::handle_exit(app, &event);
        if matches!(event, tauri::RunEvent::ExitRequested { code: Some(0), .. })
            && exam::exit_drained(app)
            && app.get_webview_window(OJ).is_some() != detected
        {
            let _ = completed.send(());
        }
    });
    if result.try_recv().is_ok() {
        if exam_smoke {
            let peer = exit_peer_result.try_recv().map_err(io::Error::other)?;
            async_runtime::block_on(async { timeout(DEADLINE, peer).await })
                .map_err(io::Error::other)?
                .map_err(io::Error::other)?;
            println!(
                "Exiting with an active exam drained the runtime and closed its monitor socket."
            );
        } else if detected {
            println!("The VM blocking screen exited without opening Accoding.");
        } else {
            println!("Closing the control window exited with Accoding open.");
        }
        println!("Desktop smoke passed.");
        Ok(())
    } else {
        Err(io::Error::other("Native desktop smoke did not complete successfully").into())
    }
}

fn start_monitor(
    requests: Arc<AtomicUsize>,
    exam_identity: Option<&Path>,
) -> io::Result<(Monitor, Option<mpsc::Receiver<exams::Connection>>)> {
    let exam_smoke = exam_identity.is_some();
    let router = Router::new().route(
        "/api/exams",
        get(move |headers: HeaderMap| {
            let requests = requests.clone();
            async move {
                requests.fetch_add(1, Ordering::Relaxed);
                exam_list(&headers, exam_smoke)
            }
        }),
    );
    let (addr, cert, connections) = if let Some(identity) = exam_identity {
        let (addr, cert, connections) = async_runtime::block_on(exams::serve(router, identity))?;
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
        let router = Router::new().route(
            "/",
            get(move || async move {
                counter.fetch_add(1, Ordering::Relaxed);
                Html("<title>Unexpected origin</title>")
            }),
        );
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

fn exam_list(headers: &HeaderMap, include_second_exam: bool) -> Response {
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let mut parts = cookie.split("; ").collect::<Vec<_>>();
    parts.sort_unstable();
    let items = match parts.as_slice() {
        ["_aux=synthetic", "connect.sid=synthetic-v1"] => vec![],
        ["_aux=synthetic", "connect.sid=synthetic-v2"] => {
            let start_at = OffsetDateTime::from_unix_timestamp(1_797_729_600).unwrap();
            let end_at = OffsetDateTime::from_unix_timestamp(1_797_733_200).unwrap();
            let summary = |id, title: &str| {
                ExamSummary::new(id, title.into(), start_at, end_at, ExamPhase::Scheduled).unwrap()
            };
            let mut items = vec![summary(exams::FIRST, EXAM_TITLE)];
            if include_second_exam {
                items.push(summary(exams::SECOND, "Second fixture exam"));
            }
            items
        }
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
    Json(ExamListResponse {
        user: AccodingUser::new(42, "test-student".into(), "Test Student".into()).unwrap(),
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
    deployed: &Desktop,
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
    wait_for(
        ui,
        "!document.querySelector('.open-exam')?.disabled",
        "exam selection in the admitted UI",
    )
    .await?;
    ui.eval("document.querySelector('.open-exam').click()")?;
    wait_for(
        ui,
        "document.querySelector('#exam-session-feedback').textContent === '考试准入检查尚未就绪，暂不能进入考试。' && document.querySelector('#current-exam').hidden",
        "exam admission still blocked without the remaining checks",
    )
    .await?;
    oj.eval("location.assign('/redirect')")?;
    expect_denial(decisions, "navigation", &foreign_url).await?;
    if hits.load(Ordering::Relaxed) != 0 {
        return Err(io::Error::other("Cross-origin navigation reached the listener").into());
    }
    oj.destroy()?;
    timeout(DEADLINE, async {
        while app.get_webview_window(OJ).is_some() {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;

    open_browser(app, deployed)?;
    let oj = app
        .get_webview_window(OJ)
        .ok_or_else(|| io::Error::other("Accoding window missing"))?;
    if let Err(error) = wait_for(
        &oj,
        "location.origin === 'https://accoding.buaa.edu.cn' && Boolean(document.querySelector('input[type=password]'))",
        "Accoding login form",
    ).await {
        if let Ok(url) = oj.url() {
            eprintln!("Accoding WebView location: {}{}", url.origin().ascii_serialization(), url.path());
        }
        return Err(error);
    }
    println!("Accoding login form loaded.");
    Ok(())
}

async fn open_fixture_browser(app: &AppHandle, ui: &WebviewWindow) -> Result<WebviewWindow, Error> {
    wait_for(
        ui,
        "document.querySelector('#inspect')?.disabled === false",
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
