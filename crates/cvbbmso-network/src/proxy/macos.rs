//! macOS adapter: the System Configuration proxy dictionary.
//!
//! `SCDynamicStoreCopyProxies` reports the same proxy dictionary that `CFNetwork`
//! consults when it routes a request, so the state matches what the controlled
//! browser does. The dictionary is keyed by the `kSCPropNetProxies` property
//! names, which are stable strings.

use core_foundation::base::CFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use system_configuration::dynamic_store::SCDynamicStoreBuilder;

use super::{ProxyFacts, ProxyState};
use crate::NetworkError;

/// Name of the System Configuration session created for one inspection.
const SESSION_NAME: &str = "cvbbmso-network";

/// Reads the current user's proxy configuration.
///
/// # Errors
///
/// Returns [`NetworkError::Platform`] when the System Configuration session
/// cannot be created.
pub(super) fn inspect() -> Result<ProxyState, NetworkError> {
    let store = SCDynamicStoreBuilder::new(SESSION_NAME)
        .build()
        .ok_or(NetworkError::Platform)?;

    let facts = match store.get_proxies() {
        Some(proxies) => collect_facts(&proxies),
        // The API returns null both when nothing is configured and when the
        // dictionary cannot be read, so the effect of a missing dictionary is
        // undetermined rather than clear.
        None => ProxyFacts {
            unreadable: true,
            ..ProxyFacts::default()
        },
    };

    Ok(facts.state())
}

/// Derives proxy facts from the proxy dictionary.
fn collect_facts(proxies: &CFDictionary<CFString, CFType>) -> ProxyFacts {
    let mut facts = ProxyFacts::default();

    // A manual proxy is a host plus an enable flag; a flag without a host
    // describes a configuration whose effect is not defined.
    for (enabled_key, target_key) in [
        ("HTTPEnable", "HTTPProxy"),
        ("HTTPSEnable", "HTTPSProxy"),
        ("SOCKSEnable", "SOCKSProxy"),
    ] {
        if enabled(proxies, enabled_key) {
            if text(proxies, target_key).is_some() {
                facts.target = true;
            } else {
                facts.incomplete = true;
            }
        }
    }

    // Proxy auto-configuration: a PAC URL that CFNetwork downloads and runs.
    if enabled(proxies, "ProxyAutoConfigEnable") {
        if text(proxies, "ProxyAutoConfigURLString").is_some() {
            facts.target = true;
        } else {
            facts.incomplete = true;
        }
    }

    // Proxy auto-discovery: the script is resolved at connection time, so
    // whether it routes anything through a proxy is not known up front.
    if enabled(proxies, "ProxyAutoDiscoveryEnable") {
        facts.discovery = true;
    }

    facts
}

/// Returns whether the boolean flag stored under `key` is set.
fn enabled(proxies: &CFDictionary<CFString, CFType>, key: &'static str) -> bool {
    read(proxies, key, |value| {
        // System Configuration stores these flags as CFNumber, but CFBoolean is
        // accepted as well instead of treating an unexpected encoding as clear.
        if let Some(number) = value.downcast::<CFNumber>() {
            return number.to_i32().map(|flag| flag != 0);
        }
        value.downcast::<CFBoolean>().map(bool::from)
    })
    .unwrap_or(false)
}

/// Returns the non-empty string stored under `key`.
fn text(proxies: &CFDictionary<CFString, CFType>, key: &'static str) -> Option<String> {
    read(proxies, key, |value| {
        let text = value.downcast::<CFString>()?.to_string();
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    })
}

/// Applies `read` to the value stored under `key`.
fn read<R>(
    proxies: &CFDictionary<CFString, CFType>,
    key: &'static str,
    read: impl FnOnce(&CFType) -> Option<R>,
) -> Option<R> {
    let key = CFString::from_static_string(key);
    proxies.find(&key).and_then(|stored| read(&stored))
}

#[cfg(test)]
mod tests {
    use super::{ProxyState, inspect};

    #[test]
    fn system_configuration_is_readable() {
        // The host may run with any proxy configuration, so only the shape of
        // the result is asserted; the test fails when the System Configuration
        // bridge itself breaks.
        assert!(matches!(
            inspect(),
            Ok(ProxyState::Clear | ProxyState::Configured | ProxyState::Unknown)
        ));
    }
}
