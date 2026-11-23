//! A separate, origin-restricted campus authentication window.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, webview::NewWindowResponse};
use url::Url;

#[cfg(windows)]
use super::platform;
use super::{Desktop, PREFLIGHT_BLOCKED, admission};
use crate::{policy::permits, preflight::Preflight};

pub(super) const WINDOW: &str = "campus-auth";

fn check_admission(app: &AppHandle) -> Result<(), &'static str> {
    if !app.state::<Preflight>().is_admitted() {
        return Err(PREFLIGHT_BLOCKED);
    }
    match app.state::<admission::Admission>().status() {
        admission::Status::Ready
        | admission::Status::Blocked(admission::Failure::MonitorUnreachable) => Ok(()),
        _ => Err("当前考试环境尚不允许打开校园网认证。"),
    }
}

#[tauri::command]
pub(super) async fn open_campus_auth(app: AppHandle) -> Result<(), &'static str> {
    check_admission(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        check_admission(&app)?;
        open(&app).map_err(|_| "无法打开校园网认证窗口，请重试。")
    })
    .await
    .map_err(|_| "无法打开校园网认证窗口，请重试。")?
}

fn open(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(WINDOW) {
        window.show()?;
        return window.set_focus();
    }
    let url = app.state::<Desktop>().campus_auth.clone();
    let origin = url.origin();
    #[cfg(test)]
    let blocked = app.state::<Desktop>().blocked.clone();
    let window = WebviewWindowBuilder::new(
        app,
        WINDOW,
        WebviewUrl::External(Url::parse("about:blank").map_err(tauri::Error::InvalidUrl)?),
    )
    .title("校园网认证")
    .inner_size(900.0, 700.0)
    .visible(false)
    .incognito(true)
    .devtools(false)
    .on_navigation(move |url| {
        let allowed = permits(&origin, url);
        #[cfg(test)]
        if !allowed && let Some(report) = &blocked {
            let _ = report.send(("campus-navigation", url.clone()));
        }
        allowed
    })
    .on_new_window(|_, _| NewWindowResponse::Deny)
    .build()?;
    let initialize = || {
        #[cfg(windows)]
        {
            platform::lock_down(&window)?;
            platform::restrict_origin(&window, url.origin())?;
        }
        window.navigate(url)?;
        window.show()?;
        window.set_focus()
    };
    if let Err(error) = initialize() {
        let _ = window.destroy();
        return Err(error);
    }
    Ok(())
}
