use std::path::PathBuf;

#[cfg(test)]
mod smoke;

use cvbbmso_accoding::{AccodingCookies, AccodingError};
use cvbbmso_protocol::ExamSummary;
use serde::Serialize;
use tauri::{
    AppHandle, Emitter, EventTarget, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    WindowEvent, Wry, async_runtime, webview::NewWindowResponse,
};
use url::Url;

use crate::{
    monitor::{Monitor, MonitorError},
    policy::{IDENTITY_URL, LOGIN_URL, permits, server_origin},
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
    fn deployed(data_dir: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            login: Url::parse(LOGIN_URL)?,
            identity: Url::parse(IDENTITY_URL)?,
            data_dir,
            monitor: Monitor::new(option_env!("CVBBMSO_SERVER_ORIGIN").and_then(server_origin)),
            #[cfg(test)]
            blocked: None,
        })
    }
}

pub fn run() -> tauri::Result<()> {
    #[cfg(test)]
    {
        if std::env::args_os().any(|arg| arg == "--native-smoke") {
            return smoke::run();
        }
        Ok(())
    }
    #[cfg(not(test))]
    builder()
        .setup(|app| {
            let data_dir = app.path().app_local_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            app.manage(Desktop::deployed(data_dir)?);
            build_ui(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
}

fn builder() -> tauri::Builder<Wry> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            open_accoding,
            inspect_session,
            list_exams
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                match window.label() {
                    UI => {
                        api.prevent_close();
                        window.app_handle().exit(0);
                    }
                    // Keep the current page and session cookies in the existing WebView.
                    OJ if window.hide().is_ok() => api.prevent_close(),
                    _ => {}
                }
            }
        })
}

fn build_ui(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(app, UI, WebviewUrl::App("index.html".into()))
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
        .build()
}

#[tauri::command]
async fn open_accoding(app: AppHandle) -> Result<(), &'static str> {
    const ERROR: &str = "无法打开 Accoding 窗口。";
    async_runtime::spawn_blocking(move || open_browser(&app, &app.state::<Desktop>()))
        .await
        .map_err(|_| ERROR)?
        .map_err(|_| ERROR)
}

fn open_browser(app: &AppHandle, desktop: &Desktop) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(OJ) {
        existing.show()?;
        return existing.set_focus();
    }

    let origin = desktop.login.origin();
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
        #[cfg(test)]
        if !allowed && let Some(report) = &blocked {
            let _ = report.send(("navigation", url.clone()));
        }
        allowed
    })
    .on_new_window(move |url, _| {
        let allowed = permits(&popup_origin, &url);
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
    let observation = match current_cookies(&app).await {
        Ok(_) => SessionObservation::CookieAvailable,
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
            MonitorError::Initialization(_) | MonitorError::Connection(_) => Self::ConnectionFailed,
            MonitorError::SignInRequired => Self::SignInRequired,
            MonitorError::Unavailable => Self::ServiceUnavailable,
            MonitorError::UnexpectedStatus(_) | MonitorError::InvalidResponse(_) => {
                Self::UnexpectedResponse
            }
        }
    }
}

#[tauri::command]
async fn list_exams(app: AppHandle) -> Result<Vec<ExamSummary>, ListExamsError> {
    let desktop = app.state::<Desktop>();
    let monitor = desktop.monitor.as_ref().map_err(ListExamsError::from)?;
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
        .map_err(|error| ListExamsError::from(&error))
}
