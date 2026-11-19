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

use axum::{
    Router,
    http::header,
    response::{AppendHeaders, Html, Redirect},
    routing::get,
};
use serde_json::Value;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
    time::{sleep, timeout},
};

use super::*;

type Error = Box<dyn std::error::Error + Send + Sync>;

const DEADLINE: Duration = Duration::from_secs(20);

pub(super) fn run() -> tauri::Result<()> {
    let (completed, result) = std::sync::mpsc::channel();
    let app = builder()
        .setup(move |app| {
            let data = tempfile::tempdir()?;
            let deployed = Desktop::deployed(data.path().to_owned())?;
            let (fixture, foreign, hits) = async_runtime::block_on(start_fixture())?;
            let base = Url::parse(&format!("http://{fixture}/"))?;
            let (blocked, mut decisions) = mpsc::unbounded_channel();
            app.manage(Desktop {
                login: base.join("user/login")?,
                identity: base.join("api/users/me")?,
                data_dir: data.path().to_owned(),
                blocked: Some(blocked),
            });
            let ui = build_ui(app.handle())?;
            let app = app.handle().clone();
            async_runtime::spawn(async move {
                let result =
                    exercise(&app, &ui, &deployed, &base, foreign, &hits, &mut decisions).await;
                if let Err(error) = result {
                    eprintln!("Desktop smoke failed: {error}");
                    let _ = completed.send(false);
                } else {
                    println!("Desktop smoke passed.");
                    let _ = completed.send(true);
                }
                app.exit(0);
                drop(data);
            });
            Ok(())
        })
        .build(tauri::generate_context!())?;
    app.run_return(|_, _| {});
    if result.try_recv() == Ok(true) {
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
                "/redirect",
                get(move || async move { Redirect::temporary(&format!("http://{foreign_addr}/")) }),
            );
        axum::serve(fixture, router).await.unwrap();
    });
    Ok((fixture_addr, foreign_addr, hits))
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
    expect_cookie(&oj, base, "connect.sid=synthetic-v2").await?;
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
        "document.querySelector('#session').textContent.includes('登录状态待监考服务核验')",
        "local session event",
    )
    .await?;
    Ok(())
}
