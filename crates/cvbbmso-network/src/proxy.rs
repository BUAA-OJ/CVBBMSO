//! System proxy inspection.
//!
//! Proxy environment variables are inspected first because they are portable and
//! cheap; the platform adapter is consulted only when they are clear. The
//! platform adapters do not share a trait: each target has exactly one
//! implementation, and the target itself is the seam.

mod env;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", windows)))]
mod other;
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "the Windows adapter is the only code that calls platform FFI"
)]
mod windows;

#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(not(any(target_os = "macos", windows)))]
use self::other as platform;
#[cfg(windows)]
use self::windows as platform;

use crate::NetworkError;

/// State of the system proxy configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyState {
    /// No system proxy, proxy auto-configuration or proxy environment variable
    /// is active.
    Clear,
    /// A system proxy, proxy auto-configuration or proxy environment variable is
    /// active.
    Configured,
    /// A proxy configuration exists but whether it applies to the monitoring
    /// server cannot be determined.
    Unknown,
}

/// Proxy facts collected from one configuration source.
///
/// Adapters translate a platform specific configuration into these facts and
/// leave the decision to [`ProxyFacts::state`], so the state rules are defined
/// once instead of once per platform.
#[expect(
    clippy::struct_excessive_bools,
    reason = "a fact bundle is a set of independent observations, each of which \
              is genuinely a flag"
)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProxyFacts {
    /// A proxy endpoint or a proxy auto-configuration script is configured with
    /// a target that would be used.
    pub(crate) target: bool,
    /// Proxy auto-discovery (WPAD) is enabled, so the configuration is resolved
    /// at connection time.
    pub(crate) discovery: bool,
    /// A proxy is enabled but no target is configured, so its effect is not
    /// defined by the configuration.
    pub(crate) incomplete: bool,
    /// Nothing could be read: either the configuration is unreadable or the
    /// target has no adapter, so no conclusion follows from it.
    pub(crate) unreadable: bool,
}

impl ProxyFacts {
    /// Returns the proxy state implied by these facts.
    ///
    /// An explicit target wins over auto-discovery: a manual proxy is in effect
    /// regardless of what a discovery script would return.
    pub(crate) const fn state(self) -> ProxyState {
        if self.target {
            ProxyState::Configured
        } else if self.discovery || self.incomplete || self.unreadable {
            ProxyState::Unknown
        } else {
            ProxyState::Clear
        }
    }
}

/// Inspects the proxy configuration of the current user.
///
/// # Errors
///
/// Returns [`NetworkError::Platform`] when the platform adapter cannot read the
/// configuration at all.
pub(crate) fn inspect() -> Result<ProxyState, NetworkError> {
    if env::proxy_configured() {
        return Ok(ProxyState::Configured);
    }
    platform::inspect()
}

#[cfg(test)]
mod tests {
    use super::{ProxyFacts, ProxyState};

    #[test]
    fn a_target_is_configured() {
        let facts = ProxyFacts {
            target: true,
            ..ProxyFacts::default()
        };
        assert_eq!(facts.state(), ProxyState::Configured);
    }

    #[test]
    fn auto_discovery_without_a_target_is_unknown() {
        let facts = ProxyFacts {
            discovery: true,
            ..ProxyFacts::default()
        };
        assert_eq!(facts.state(), ProxyState::Unknown);
    }

    #[test]
    fn an_incomplete_proxy_is_unknown() {
        let facts = ProxyFacts {
            incomplete: true,
            ..ProxyFacts::default()
        };
        assert_eq!(facts.state(), ProxyState::Unknown);
    }

    #[test]
    fn an_unreadable_configuration_is_unknown() {
        let facts = ProxyFacts {
            unreadable: true,
            ..ProxyFacts::default()
        };
        assert_eq!(facts.state(), ProxyState::Unknown);
    }

    #[test]
    fn nothing_configured_is_clear() {
        assert_eq!(ProxyFacts::default().state(), ProxyState::Clear);
    }

    #[test]
    fn a_target_wins_over_auto_discovery() {
        let facts = ProxyFacts {
            target: true,
            discovery: true,
            incomplete: true,
            unreadable: true,
        };
        assert_eq!(facts.state(), ProxyState::Configured);
    }
}
