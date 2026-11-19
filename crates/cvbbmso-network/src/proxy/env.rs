//! Proxy environment variables.

/// Proxy environment variables read by HTTP clients, in both spellings.
///
/// Lowercase names are checked as well because some shells and container images
/// set them; on Windows variable names are compared case-insensitively, so the
/// duplicate entries are harmless there.
const PROXY_VARIABLES: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// Returns `true` when a proxy environment variable holds a non-empty value.
pub(super) fn proxy_configured() -> bool {
    configured(|name| std::env::var(name).ok())
}

/// Implementation of [`proxy_configured`] with an injectable variable lookup.
fn configured(mut lookup: impl FnMut(&str) -> Option<String>) -> bool {
    PROXY_VARIABLES
        .iter()
        .any(|name| lookup(name).is_some_and(|value| !value.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::configured;

    #[test]
    fn missing_variables_are_not_configured() {
        assert!(!configured(|_| None));
    }

    #[test]
    fn empty_and_blank_variables_are_not_configured() {
        assert!(!configured(|_| Some(String::new())));
        assert!(!configured(|_| Some("  \t ".to_owned())));
    }

    #[test]
    fn lowercase_variable_is_configured() {
        let configured_state =
            configured(|name| (name == "https_proxy").then(|| "http://proxy:8080".to_owned()));
        assert!(configured_state);
    }

    #[test]
    fn all_proxy_variable_is_configured() {
        let configured_state =
            configured(|name| (name == "ALL_PROXY").then(|| "socks5://proxy:1080".to_owned()));
        assert!(configured_state);
    }
}
