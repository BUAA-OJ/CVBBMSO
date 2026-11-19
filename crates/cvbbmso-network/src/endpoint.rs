//! Reachability probe for the monitoring server.

use std::time::Duration;

use url::Url;

use crate::NetworkError;

/// Reachability of the monitoring server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointState {
    /// The server answered a health request.
    Reachable,
    /// The server did not answer a health request.
    Unreachable,
}

/// Path of the health endpoint used for the probe.
const HEALTH_PATH: &str = "/api/health";

/// Upper bound for establishing a TCP and TLS connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Upper bound for the whole probe, including the connection and the response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Probes the health endpoint of `origin`.
///
/// # Errors
///
/// Returns [`NetworkError::Io`] when the HTTP client cannot be initialised. A
/// request that never reaches the server is reported as
/// [`EndpointState::Unreachable`], not as an error. An origin whose scheme is not
/// `http` or `https` is reported as [`EndpointState::Unreachable`] without any
/// request, because such an origin cannot be probed.
pub(crate) async fn probe(origin: &Url) -> Result<EndpointState, NetworkError> {
    let Some(url) = health_url(origin) else {
        return Ok(EndpointState::Unreachable);
    };

    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        // A proxied answer says nothing about direct reachability, and the exam
        // forbids proxies; probe the server directly.
        .no_proxy()
        .build()
        .map_err(|_| NetworkError::Io)?;

    // Any completed exchange means the server answered. The status code is
    // irrelevant here: reachability is the question, authorisation is not.
    Ok(match client.get(url).send().await {
        Ok(_) => EndpointState::Reachable,
        Err(_) => EndpointState::Unreachable,
    })
}

/// Returns the health URL of `origin`, or `None` when the origin is not HTTP(S).
///
/// A path other than `/` is kept as a prefix, so an origin mounted below a
/// reverse-proxy path stays probeable.
fn health_url(origin: &Url) -> Option<Url> {
    if !matches!(origin.scheme(), "http" | "https") {
        return None;
    }

    let mut url = origin.clone();
    url.set_query(None);
    url.set_fragment(None);
    let prefix = url.path().trim_end_matches('/');
    url.set_path(&format!("{prefix}{HEALTH_PATH}"));
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::health_url;
    use url::Url;

    /// Parses a URL that the tests below are known to pass.
    fn parse(value: &str) -> Url {
        Url::parse(value).expect("test URL must parse")
    }

    #[test]
    fn root_origin_uses_the_health_endpoint() {
        let url = health_url(&parse("https://monitor.example.com")).expect("HTTP origin");
        assert_eq!(url.as_str(), "https://monitor.example.com/api/health");
    }

    #[test]
    fn trailing_slash_is_not_duplicated() {
        let url = health_url(&parse("https://monitor.example.com/")).expect("HTTP origin");
        assert_eq!(url.as_str(), "https://monitor.example.com/api/health");
    }

    #[test]
    fn path_prefix_is_preserved() {
        let url = health_url(&parse("https://monitor.example.com/exams/")).expect("HTTP origin");
        assert_eq!(url.as_str(), "https://monitor.example.com/exams/api/health");
    }

    #[test]
    fn query_and_fragment_are_dropped() {
        let url = health_url(&parse("https://monitor.example.com/?a=1#b")).expect("HTTP origin");
        assert_eq!(url.as_str(), "https://monitor.example.com/api/health");
    }

    #[test]
    fn non_http_origin_cannot_be_probed() {
        assert!(health_url(&parse("file:///tmp/health")).is_none());
    }
}
