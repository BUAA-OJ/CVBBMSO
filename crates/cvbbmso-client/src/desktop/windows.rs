use std::{io, sync::mpsc, time::Duration};

use tauri::WebviewWindow;
use url::Url;
use webview2_com::{
    LaunchingExternalUriSchemeEventHandler,
    Microsoft::Web::WebView2::Win32::{ICoreWebView2_18, ICoreWebView2Settings3},
};
use windows_core::Interface;

pub(super) fn lock_down(browser: &WebviewWindow) -> tauri::Result<()> {
    let (send, receive) = mpsc::channel();
    browser.with_webview(move |native| {
        let set_options = || -> windows_core::Result<()> {
            // SAFETY: WebView2 is live and this callback executes on its owning UI thread.
            unsafe {
                let webview = native.controller().CoreWebView2()?;
                let settings = webview.Settings()?;
                settings.SetAreDevToolsEnabled(false)?;
                settings
                    .cast::<ICoreWebView2Settings3>()?
                    .SetAreBrowserAcceleratorKeysEnabled(false)?;
                webview
                    .cast::<ICoreWebView2_18>()?
                    .add_LaunchingExternalUriScheme(
                        &LaunchingExternalUriSchemeEventHandler::create(Box::new(|_, args| {
                            if let Some(args) = args {
                                args.SetCancel(true)?;
                            }
                            Ok(())
                        })),
                        &mut 0,
                    )?;
            }
            Ok(())
        };
        let _ = send.send(set_options());
    })?;
    receive
        .recv_timeout(Duration::from_secs(10))
        .map_err(io::Error::other)?
        .map_err(|error| io::Error::other(error).into())
}

pub(super) fn cookie_pairs(
    browser: &WebviewWindow,
    identity: Url,
) -> tauri::Result<Vec<(String, String)>> {
    Ok(browser
        .cookies_for_url(identity)?
        .into_iter()
        .map(|cookie| (cookie.name().to_owned(), cookie.value().to_owned()))
        .collect())
}

#[cfg(test)]
pub(super) fn assert_locked(browser: &WebviewWindow) -> tauri::Result<()> {
    let (send, receive) = mpsc::channel();
    browser.with_webview(move |native| {
        let read_options = || -> windows_core::Result<(bool, bool)> {
            // SAFETY: WebView2 is live and this callback executes on its owning UI thread.
            unsafe {
                let webview = native.controller().CoreWebView2()?;
                let settings = webview.Settings()?;
                let mut devtools = Default::default();
                let mut accelerators = Default::default();
                settings.AreDevToolsEnabled(&mut devtools)?;
                settings
                    .cast::<ICoreWebView2Settings3>()?
                    .AreBrowserAcceleratorKeysEnabled(&mut accelerators)?;
                Ok((devtools.as_bool(), accelerators.as_bool()))
            }
        };
        let _ = send.send(read_options());
    })?;
    if receive
        .recv_timeout(Duration::from_secs(10))
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?
        != (false, false)
    {
        return Err(
            io::Error::other("WebView2 debugging or browser accelerators are enabled").into(),
        );
    }
    Ok(())
}
