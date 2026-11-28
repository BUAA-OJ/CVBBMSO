use std::{io, path::PathBuf};

mod admission;
mod exam;
#[cfg(test)]
mod smoke;

use cvbbmso_accoding::{AccodingCookies, AccodingError};
use cvbbmso_exam::ExamError;
use cvbbmso_protocol::ExamSummary;
use reqwest::StatusCode;
use serde::Serialize;
use tauri::{
    AppHandle, Emitter, EventTarget, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    WindowEvent, Wry, async_runtime, webview::NewWindowResponse,
};
use url::Url;

use crate::{
    host_probe,
    monitor::{Monitor, MonitorError},
    policy::{IDENTITY_URL, LOGIN_URL, permits, server_origin},
    preflight::{Preflight, Verdict},
};

#[cfg_attr(target_os = "macos", path = "desktop/macos.rs")]
#[cfg_attr(windows, path = "desktop/windows.rs")]
mod platform;

const UI: &str = "student";
const OJ: &str = "accoding";
const SESSION_EVENT: &str = "accoding-session";

struct Desktop {
    login: Url,
    identity: Url,
    data_dir: PathBuf,
    monitor: Result<Monitor, MonitorError>,
    #[cfg(test)]
    blocked: Option<tokio::sync::mpsc::UnboundedSender<(&'static str, Url)>>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionObservation {
    CookieAvailable,
    CookieMissing,
}

impl Desktop {
    fn deployed(
        data_dir: PathBuf,
        raw_origin: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let origin = raw_origin
            .map(|raw| {
                server_origin(raw).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "CVBBMSO_SERVER_ORIGIN must be an HTTPS origin",
                    )
                })
            })
            .transpose()?;
        Ok(Self {
            login: Url::parse(LOGIN_URL)?,
            identity: Url::parse(IDENTITY_URL)?,
            data_dir,
            monitor: Monitor::new(origin),
            #[cfg(test)]
            blocked: None,
        })
    }
}

pub fn run() -> tauri::Result<()> {
    #[cfg(test)]
    {
        #[cfg(target_os = "macos")]
        admission::fixture::check_startup_network_recovery();
        match std::env::args().nth(1).as_deref() {
            Some("--native-smoke") => return smoke::run(smoke::Case::Desktop, None),
            Some("--native-smoke-detected") => return smoke::run(smoke::Case::Detected, None),
            Some("--native-admission-exit") => {
                return smoke::run(smoke::Case::ExitDuringInstall, None);
            }
            Some("--native-admission-cleanup-failure") => {
                return smoke::run(smoke::Case::CleanupFailure, None);
            }
            Some("--native-exam-smoke") => return smoke::exams::launch(),
            Some("--native-recording-smoke") => return smoke::exams::launch_recording(),
            Some("--native-exam-smoke-child") => {
                let identity = std::env::args_os().nth(2).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "Missing fixture TLS identity")
                })?;
                let mode = std::env::args_os().nth(3);
                let case = match mode.as_deref().and_then(std::ffi::OsStr::to_str) {
                    Some("guard-loss") => smoke::Case::GuardLoss,
                    Some("recording") => smoke::Case::Recording,
                    Some("restored-recording") => smoke::Case::RestoredRecording,
                    _ => smoke::Case::Exam,
                };
                return smoke::run(case, Some(PathBuf::from(identity)));
            }
            Some("--native-vm-probe") => {
                let start = std::time::Instant::now();
                let verdict = async_runtime::block_on(host_probe::detect());
                println!(
                    "Native VM probe completed in {} ms.",
                    start.elapsed().as_millis()
                );
                println!(
                    "{}",
                    serde_json::to_string(&verdict).map_err(io::Error::other)?
                );
            }
            _ => {}
        }
        Ok(())
    }
    #[cfg(not(test))]
    {
        #[cfg(target_os = "macos")]
        if platform::is_root() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "CVBBMSO must run as the current console user",
            )
            .into());
        }
        let app = builder()
            .setup(|app| {
                let data_dir = app.path().app_local_data_dir()?;
                app.manage(Desktop::deployed(
                    data_dir,
                    option_env!("CVBBMSO_SERVER_ORIGIN"),
                )?);
                build_ui(app.handle())?;
                Ok(())
            })
            .build(tauri::generate_context!())?;
        let handle = app.handle().clone();
        let runtime_code = app.run_return(|app, event| exam::handle_exit(app, &event));
        let code = exam::exit_code(&handle, runtime_code);
        if code != 0 {
            std::process::exit(code);
        }
        Ok(())
    }
}

fn builder() -> tauri::Builder<Wry> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            vm_preflight,
            admission::admission_status,
            #[cfg(target_os = "macos")]
            admission::authorize_macos_egress,
            open_accoding,
            inspect_session,
            list_exams,
            exam::open_exam,
            exam::get_exam_status,
            exam::close_exam,
            confirm_exit,
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                match window.label() {
                    UI => {
                        api.prevent_close();
                        #[cfg(target_os = "macos")]
                        if window
                            .app_handle()
                            .state::<admission::Admission>()
                            .is_authorizing()
                        {
                            return;
                        }
                        let _ = window.app_handle().emit_to(
                            EventTarget::webview(UI),
                            "confirm-exit",
                            (),
                        );
                    }
                    // Keep the current page and session cookies in the existing WebView.
                    OJ if window.hide().is_ok() => api.prevent_close(),
                    _ => {}
                }
            }
        })
}

#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri injects the application handle by value"
)]
fn confirm_exit(app: AppHandle) -> Result<(), exam::ExamCommandError> {
    if !app.state::<exam::ExamSession>().can_close() {
        return Err(exam::ExamCommandError::ReportingPending);
    }
    app.exit(0);
    Ok(())
}

fn build_ui(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    app.manage(Preflight::new());
    admission::Admission::launch(app);
    exam::ExamSession::launch(app);
    let window = WebviewWindowBuilder::new(app, UI, WebviewUrl::App("index.html".into()))
        .title("CVBBMSO")
        .inner_size(560.0, 540.0)
        .min_inner_size(340.0, 320.0)
        .devtools(false)
        .on_navigation(|url| {
            // Both local origins belong to Tauri; the app has no remote frontend.
            let origin = if cfg!(windows) {
                ("http", Some("tauri.localhost"))
            } else {
                ("tauri", Some("localhost"))
            };
            (url.scheme(), url.host_str()) == origin
                && url.port().is_none()
                && matches!(url.path(), "" | "/" | "/index.html")
        })
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .build()?;
    schedule_preflight(app, false);
    Ok(window)
}

fn schedule_preflight(app: &AppHandle, retry: bool) {
    let probe = {
        #[cfg(test)]
        let app = app.clone();
        async move {
            #[cfg(test)]
            if let Some(probe) = app.try_state::<smoke::FakeProbe>() {
                return probe.next();
            }
            host_probe::detect().await
        }
    };
    if let Some(work) = app.state::<Preflight>().schedule(retry, probe) {
        let app = app.clone();
        async_runtime::spawn(async move {
            work.await;
            app.state::<admission::Admission>().start(&app, false);
        });
    }
}

#[tauri::command]
async fn vm_preflight(app: AppHandle, retry: bool) -> Verdict {
    if retry {
        schedule_preflight(&app, true);
    }
    app.state::<Preflight>().verdict().await
}

const PREFLIGHT_BLOCKED: &str = "设备检查通过后才能继续。";

#[tauri::command]
async fn open_accoding(app: AppHandle) -> Result<(), &'static str> {
    const ERROR: &str = "无法打开 Accoding 窗口。";
    if !app.state::<Preflight>().is_admitted() {
        return Err(PREFLIGHT_BLOCKED);
    }
    app.state::<admission::Admission>().require_ready()?;
    async_runtime::spawn_blocking(move || {
        app.state::<admission::Admission>().require_ready()?;
        open_browser(&app, &app.state::<Desktop>()).map_err(|_| ERROR)
    })
    .await
    .map_err(|_| ERROR)?
}

fn open_browser(app: &AppHandle, desktop: &Desktop) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(OJ) {
        if app.state::<exam::ExamSession>().needs_sign_in() {
            existing.navigate(desktop.login.clone())?;
        }
        existing.show()?;
        return existing.set_focus();
    }

    std::fs::create_dir_all(&desktop.data_dir)?;
    let origin = desktop.login.origin();
    let navigation_app = app.clone();
    let popup_origin = origin.clone();
    let popup_app = app.clone();
    #[cfg(test)]
    let blocked = desktop.blocked.clone();
    #[cfg(test)]
    let blocked_popup = blocked.clone();
    let browser = WebviewWindowBuilder::new(
        app,
        OJ,
        WebviewUrl::External(Url::parse("about:blank").map_err(tauri::Error::InvalidUrl)?),
    )
    .title("Accoding")
    .inner_size(1120.0, 780.0)
    .visible(false)
    .incognito(cfg!(test))
    .data_directory(desktop.data_dir.join("accoding"))
    .devtools(false)
    .on_navigation(move |url| {
        let allowed = permits(&origin, url);
        if !allowed {
            report_denied_origin(&navigation_app, &origin, url);
        }
        #[cfg(test)]
        if !allowed && let Some(report) = &blocked {
            let _ = report.send(("navigation", url.clone()));
        }
        allowed
    })
    .on_new_window(move |url, _| {
        let allowed = permits(&popup_origin, &url);
        if !allowed {
            report_denied_origin(&popup_app, &popup_origin, &url);
        }
        #[cfg(test)]
        if !allowed && let Some(report) = &blocked_popup {
            let _ = report.send(("popup", url.clone()));
        }
        if allowed {
            let app = popup_app.clone();
            // Reuse the restricted WebView for same-origin target=_blank links.
            let _ = popup_app.run_on_main_thread(move || {
                if let Some(browser) = app.get_webview_window(OJ) {
                    let _ = browser.navigate(url);
                }
            });
        }
        NewWindowResponse::Deny
    })
    .build()?;

    let initialize = || {
        #[cfg(windows)]
        platform::lock_down(&browser)?;
        browser.navigate(desktop.login.clone())?;
        browser.show()?;
        browser.set_focus()
    };
    if let Err(error) = initialize() {
        let _ = browser.destroy();
        return Err(error);
    }
    Ok(())
}

fn report_denied_origin(app: &AppHandle, allowed: &url::Origin, url: &Url) {
    if let Some(sessions) = app.try_state::<exam::ExamSession>() {
        sessions.navigation_denied(allowed, url);
    }
}

fn read_cookies(
    browser: &WebviewWindow,
    identity: Url,
) -> Result<AccodingCookies, ReadSessionError> {
    let pairs = platform::cookie_pairs(browser, identity).map_err(|_| ReadSessionError::Native)?;
    AccodingCookies::from_pairs(pairs).map_err(ReadSessionError::Invalid)
}

enum ReadSessionError {
    BrowserClosed,
    Native,
    Invalid(AccodingError),
}

async fn current_cookies(app: &AppHandle) -> Result<AccodingCookies, ReadSessionError> {
    let browser = app
        .get_webview_window(OJ)
        .ok_or(ReadSessionError::BrowserClosed)?;
    let identity = app.state::<Desktop>().identity.clone();
    async_runtime::spawn_blocking(move || read_cookies(&browser, identity))
        .await
        .map_err(|_| ReadSessionError::Native)?
}

#[tauri::command]
async fn inspect_session(app: AppHandle) -> Result<(), &'static str> {
    if !app.state::<Preflight>().is_admitted() {
        return Err(PREFLIGHT_BLOCKED);
    }
    app.state::<admission::Admission>().require_ready()?;
    let retry = app.state::<exam::ExamSession>().cookie_retry();
    let observation = match current_cookies(&app).await {
        Ok(cookies) => {
            if let Some(retry) = retry {
                retry.submit(cookies).await;
            }
            SessionObservation::CookieAvailable
        }
        Err(ReadSessionError::Invalid(AccodingError::MissingSession)) => {
            SessionObservation::CookieMissing
        }
        Err(ReadSessionError::BrowserClosed) => return Err("请先打开 Accoding 窗口。"),
        Err(ReadSessionError::Invalid(_)) => return Err("Accoding 返回了无效的会话。"),
        Err(ReadSessionError::Native) => return Err("无法读取 Accoding 会话。"),
    };
    app.emit_to(EventTarget::webview(UI), SESSION_EVENT, observation)
        .map_err(|_| "无法更新会话状态。")
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ListExamsError {
    PreflightBlocked,
    SignInRequired,
    SessionUnreadable,
    MonitorUnconfigured,
    ConnectionFailed,
    ServiceUnavailable,
    UnexpectedResponse,
}

impl From<&MonitorError> for ListExamsError {
    fn from(error: &MonitorError) -> Self {
        match error {
            MonitorError::Configuration => Self::MonitorUnconfigured,
            MonitorError::Initialization(_) | MonitorError::Request(ExamError::Http(Some(_))) => {
                Self::ConnectionFailed
            }
            MonitorError::Request(ExamError::Auth) => Self::SignInRequired,
            MonitorError::Request(ExamError::HttpStatus(StatusCode::SERVICE_UNAVAILABLE)) => {
                Self::ServiceUnavailable
            }
            MonitorError::Request(_) => Self::UnexpectedResponse,
        }
    }
}

#[tauri::command]
async fn list_exams(app: AppHandle) -> Result<Vec<ExamSummary>, ListExamsError> {
    if !app.state::<Preflight>().is_admitted() {
        return Err(ListExamsError::PreflightBlocked);
    }
    let desktop = app.state::<Desktop>();
    let monitor = match desktop.monitor.as_ref() {
        Err(MonitorError::Configuration) => return Err(ListExamsError::MonitorUnconfigured),
        _ if !app.state::<admission::Admission>().is_ready() => {
            return Err(ListExamsError::PreflightBlocked);
        }
        monitor => monitor.map_err(ListExamsError::from)?,
    };
    let cookies = current_cookies(&app).await.map_err(|error| match error {
        ReadSessionError::BrowserClosed
        | ReadSessionError::Invalid(AccodingError::MissingSession) => {
            ListExamsError::SignInRequired
        }
        ReadSessionError::Native | ReadSessionError::Invalid(_) => {
            ListExamsError::SessionUnreadable
        }
    })?;
    monitor
        .list_exams(&cookies)
        .await
        .map(|response| response.items)
        .map_err(|error| ListExamsError::from(&error))
}
