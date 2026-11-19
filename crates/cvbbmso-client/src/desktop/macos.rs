use std::{io, ptr::NonNull, sync::mpsc, time::Duration};

use block2::RcBlock;
use objc2_foundation::{NSArray, NSHTTPCookie};
use objc2_web_kit::WKWebView;
use tauri::WebviewWindow;
use url::Url;

#[path = "macos/cookie_scope.rs"]
mod cookie_scope;

use cookie_scope::matches_cookie;

pub(super) fn cookie_pairs(
    browser: &WebviewWindow,
    identity: Url,
) -> tauri::Result<Vec<(String, String)>> {
    // Tauri's URL helper uses Wry's exact-domain lookup on WebKit. Read the
    // original domain and path to also handle valid parent-domain cookies.
    let (send, receive) = mpsc::channel();
    browser.with_webview(move |native| {
        // SAFETY: Tauri supplies a live WKWebView on its UI thread for this callback.
        unsafe {
            let webview = &*native.inner().cast::<WKWebView>();
            let store = webview.configuration().websiteDataStore().httpCookieStore();
            store.getAllCookies(&RcBlock::new(
                move |cookies: NonNull<NSArray<NSHTTPCookie>>| {
                    let pairs = cookies
                        .as_ref()
                        .to_vec()
                        .into_iter()
                        .filter(|cookie| {
                            matches_cookie(
                                &identity,
                                Some(&cookie.domain().to_string()),
                                Some(&cookie.path().to_string()),
                                cookie.isSecure(),
                            ) && cookie
                                .expiresDate()
                                .is_none_or(|date| date.timeIntervalSinceNow() > 0.0)
                        })
                        .map(|cookie| (cookie.name().to_string(), cookie.value().to_string()))
                        .collect();
                    let _ = send.send(pairs);
                },
            ));
        }
    })?;
    receive
        .recv_timeout(Duration::from_secs(10))
        .map_err(|error| io::Error::other(error).into())
}

#[cfg(test)]
pub(super) fn assert_locked(browser: &WebviewWindow) -> tauri::Result<()> {
    let (send, receive) = mpsc::channel();
    browser.with_webview(move |native| {
        // SAFETY: The smoke environment runs macOS 13.3 or later. This is the
        // live WKWebView on its owning UI thread.
        let inspectable = unsafe { (&*native.inner().cast::<WKWebView>()).isInspectable() };
        let _ = send.send(inspectable);
    })?;
    if receive
        .recv_timeout(Duration::from_secs(10))
        .map_err(io::Error::other)?
    {
        return Err(io::Error::other("WKWebView inspection is enabled").into());
    }
    Ok(())
}
