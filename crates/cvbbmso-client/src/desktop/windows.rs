use std::{io, ptr::null_mut, sync::mpsc, time::Duration};

use tauri::WebviewWindow;
use url::{Origin, Url};
use webview2_com::{
    LaunchingExternalUriSchemeEventHandler,
    Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL, COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
        ICoreWebView2_18, ICoreWebView2_22, ICoreWebView2Settings3,
    },
    WebResourceRequestedEventHandler, take_pwstr,
};
use windows_core::{Interface, PWSTR, w};
use windows_sys::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, SECURITY_MAX_SID_SIZE, WinBuiltinAdministratorsSid,
};

pub(super) fn is_elevated() -> bool {
    let mut sid = [0_u32; SECURITY_MAX_SID_SIZE as usize / size_of::<u32>()];
    let mut length = SECURITY_MAX_SID_SIZE;
    let mut member = 0;
    // SAFETY: sid is a writable, DWORD-aligned SECURITY_MAX_SID_SIZE buffer.
    // CheckTokenMembership reads it only after CreateWellKnownSid succeeded.
    // Its null handle requests the effective current thread/process token.
    unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            null_mut(),
            sid.as_mut_ptr().cast(),
            &raw mut length,
        ) != 0
            && CheckTokenMembership(null_mut(), sid.as_mut_ptr().cast(), &raw mut member) != 0
            && member != 0
    }
}

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
                let mut handler_token = 0;
                webview
                    .cast::<ICoreWebView2_18>()?
                    .add_LaunchingExternalUriScheme(
                        &LaunchingExternalUriSchemeEventHandler::create(Box::new(|_, args| {
                            if let Some(args) = args {
                                args.SetCancel(true)?;
                            }
                            Ok(())
                        })),
                        &raw mut handler_token,
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

/// Blocks requests outside the configured page origin before they reach the network.
pub(super) fn restrict_origin(browser: &WebviewWindow, origin: Origin) -> tauri::Result<()> {
    let (send, receive) = mpsc::channel();
    browser.with_webview(move |native| {
        let configure = || -> windows_core::Result<()> {
            // SAFETY: the WebView and environment are live on their owning UI
            // thread; request handlers and response creation stay on that thread.
            unsafe {
                let webview = native.controller().CoreWebView2()?;
                let environment = native.environment();
                // The legacy overload does not reliably intercept iframe requests.
                webview
                    .cast::<ICoreWebView2_22>()?
                    .AddWebResourceRequestedFilterWithRequestSourceKinds(
                        w!("*"),
                        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                        COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
                    )?;
                let mut token = 0;
                webview.add_WebResourceRequested(
                    &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                        if let Some(args) = args {
                            let mut uri = PWSTR::null();
                            args.Request()?.Uri(&raw mut uri)?;
                            let uri = take_pwstr(uri);
                            if !Url::parse(&uri)
                                .is_ok_and(|url| crate::policy::permits(&origin, &url))
                            {
                                let response = environment.CreateWebResourceResponse(
                                    None,
                                    403,
                                    w!("Forbidden"),
                                    w!("Content-Length: 0\r\n"),
                                )?;
                                args.SetResponse(&response)?;
                            }
                        }
                        Ok(())
                    })),
                    &raw mut token,
                )?;
            }
            Ok(())
        };
        let _ = send.send(configure());
    })?;
    receive
        .recv_timeout(Duration::from_secs(10))
        .map_err(io::Error::other)?
        .map_err(|error| io::Error::other(error).into())
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
                let mut devtools = windows_core::BOOL::default();
                let mut accelerators = windows_core::BOOL::default();
                settings.AreDevToolsEnabled(&raw mut devtools)?;
                settings
                    .cast::<ICoreWebView2Settings3>()?
                    .AreBrowserAcceleratorKeysEnabled(&raw mut accelerators)?;
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
