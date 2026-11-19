//! Native smoke driver compiled into the test executable only.

use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

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

type Error = Box<dyn std::error::Error + Send + Sync>;

const DEADLINE: Duration = Duration::from_secs(20);
const EXAM_TITLE: &str = "<b>Fixture exam</b>";

pub(super) fn run() -> tauri::Result<()> {
    let data = tempfile::tempdir()?;
    let data_dir = data.path().to_owned();
    let (completed, result) = std::sync::mpsc::channel();
    let app = builder()
        .setup(move |app| {
            let deployed = Desktop::deployed(data_dir.clone())?;
            let (fixture, foreign, hits) = async_runtime::block_on(start_fixture())?;
            let (monitor_addr, cert, _) = async_runtime::block_on(tls::serve(
                Router::new().route("/api/exams", get(exam_list)),
            ));
            let monitor_origin = Url::parse(&format!("https://{monitor_addr}"))?.origin();
            let monitor = Monitor::with_root_certificate(
                monitor_origin,
                reqwest::Certificate::from_der(cert.as_ref())?,
            );
            let base = Url::parse(&format!("http://{fixture}/"))?;
            let (blocked, mut decisions) = mpsc::unbounded_channel();
            app.manage(Desktop {
                login: base.join("user/login")?,
                identity: base.join("api/users/me")?,
                data_dir,
                monitor,
                blocked: Some(blocked),
            });
            let ui = build_ui(app.handle())?;
            let app = app.handle().clone();
            async_runtime::spawn(async move {
                let exercised =
                    exercise(&app, &ui, &deployed, &base, foreign, &hits, &mut decisions).await;
                if let Err(error) = exercised.and_then(|()| ui.close().map_err(Into::into)) {
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
        if matches!(event, tauri::RunEvent::ExitRequested { code: Some(0), .. })
            && app.get_webview_window(OJ).is_some()
        {
            let _ = completed.send(());
        }
    });
    if result.try_recv().is_ok() {
        println!("Closing the control window exited with Accoding open.");
        println!("Desktop smoke passed.");
        Ok(())
    } else {
        Err(io::Error::other("Native desktop smoke did not complete successfully").into())
    }
}

async fn start_fixture() -> io::Result<(SocketAddr, SocketAddr, Arc<AtomicUsize>)> {
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
                get(|| async {
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
                "/redirect",
                get(move || async move { Redirect::temporary(&format!("http://{foreign_addr}/")) }),
            );
        axum::serve(fixture, router).await.unwrap();
    });
    Ok((fixture_addr, foreign_addr, hits))
}

async fn exam_list(headers: HeaderMap) -> Response {
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
            vec![
                ExamSummary::new(
                    Uuid::from_u128(0x10),
                    EXAM_TITLE.into(),
                    start_at,
                    end_at,
                    ExamPhase::Scheduled,
                )
                .unwrap(),
            ]
        }
        ["_aux=synthetic", "connect.sid=unavailable"] => {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
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
        "Exam list loaded and refreshed through HTTPS; empty, expired and unavailable states verified."
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
                invoke("inspect_session"),
                invoke("list_exams"),
                invoke("plugin:event|listen", {
                    event: "accoding-session",
                    target: { kind: "Any" },
                    handler: window.__TAURI_INTERNALS__.transformCallback(() => {})
                })
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
