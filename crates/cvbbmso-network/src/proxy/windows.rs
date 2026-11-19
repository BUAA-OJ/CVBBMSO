//! Windows adapter: the current user's `WinHTTP` / Internet Explorer proxy
//! configuration.
//!
//! `WinHttpGetIEProxyConfigForCurrentUser` exposes the registry-backed Internet
//! Explorer settings that `WinInet` uses for HTTP requests, including the manual
//! proxy list, the proxy auto-configuration URL and the auto-detect flag.
//! `WinInet` reads the same settings, so a separate `WinInet` query would only
//! duplicate them; only the settings that `WinInet` keeps per connection
//! (dial-up and VPN profiles) are not covered.

use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WinHttpGetIEProxyConfigForCurrentUser,
};
use windows_sys::core::PWSTR;

use super::{ProxyFacts, ProxyState};
use crate::NetworkError;

/// Longest string accepted from the platform, in UTF-16 code units.
///
/// The platform returns NUL-terminated strings; the limit keeps a malformed
/// pointer from turning into an unbounded scan.
const MAX_TEXT_LEN: usize = 8 * 1024;

/// Reads the current user's proxy configuration.
///
/// # Errors
///
/// Returns [`NetworkError::Platform`] when the platform call fails.
pub(super) fn inspect() -> Result<ProxyState, NetworkError> {
    let config = IeProxyConfig::load()?;

    let mut facts = ProxyFacts::default();
    // SAFETY: `config` owns the strings for as long as it is alive, and the
    // helpers only read them.
    unsafe {
        facts.discovery = config.raw.fAutoDetect != 0;
        facts.target =
            text(config.raw.lpszAutoConfigUrl).is_some() || text(config.raw.lpszProxy).is_some();
    }

    Ok(facts.state())
}

/// Owns the strings returned by `WinHttpGetIEProxyConfigForCurrentUser`.
struct IeProxyConfig {
    raw: WINHTTP_CURRENT_USER_IE_PROXY_CONFIG,
}

impl IeProxyConfig {
    /// Queries the configuration of the current user.
    fn load() -> Result<Self, NetworkError> {
        let mut raw = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        // SAFETY: `raw` is a writable structure with the layout the API expects
        // and lives for the duration of the call.
        let succeeded = unsafe { WinHttpGetIEProxyConfigForCurrentUser(&raw mut raw) };
        if succeeded == 0 {
            return Err(NetworkError::Platform);
        }
        Ok(Self { raw })
    }
}

impl Drop for IeProxyConfig {
    fn drop(&mut self) {
        // SAFETY: the three fields are null or strings allocated by `WinHTTP` for
        // this structure; each is released exactly once.
        unsafe {
            free(self.raw.lpszAutoConfigUrl);
            free(self.raw.lpszProxy);
            free(self.raw.lpszProxyBypass);
        }
    }
}

/// Releases one string returned in `WINHTTP_CURRENT_USER_IE_PROXY_CONFIG`.
///
/// # Safety
///
/// `value` must be null or a string that `WinHTTP` allocated for a proxy
/// configuration structure that has not been freed yet.
unsafe fn free(value: PWSTR) {
    if !value.is_null() {
        // SAFETY: `WinHTTP` documents these strings as allocated with `GlobalAlloc`,
        // so GlobalFree releases them.
        unsafe { GlobalFree(value.cast()) };
    }
}

/// Copies a NUL-terminated UTF-16 string owned by `WinHTTP`.
///
/// # Safety
///
/// `value` must be null or point to a NUL-terminated UTF-16 string that stays
/// valid for the duration of the call.
unsafe fn text(value: PWSTR) -> Option<String> {
    if value.is_null() {
        return None;
    }

    let mut len = 0;
    while len < MAX_TEXT_LEN {
        // SAFETY: the caller guarantees a NUL-terminated string, and the loop
        // stops at the terminator or at the length cap.
        if unsafe { *value.add(len) } == 0 {
            break;
        }
        len += 1;
    }

    // SAFETY: the first `len` code units were read above and belong to the same
    // string, which the caller guarantees to be valid.
    let units = unsafe { std::slice::from_raw_parts(value, len) };
    let text = String::from_utf16_lossy(units);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}
