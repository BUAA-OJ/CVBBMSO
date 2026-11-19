//! Contract tests for [`Inspector::check`] against a local HTTP server.
//!
//! The proxy state depends on the host that runs the tests, so it is only
//! checked for being a known state; the endpoint state is fully controlled here.

use cvbbmso_network::{EndpointState, Inspector, ProxyState};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Parses a URL that the tests below are known to pass.
fn parse(value: &str) -> Url {
    Url::parse(value).expect("test URL must parse")
}

/// Asserts that the proxy state is one of the documented states.
fn assert_known_proxy_state(state: ProxyState) {
    assert!(matches!(
        state,
        ProxyState::Clear | ProxyState::Configured | ProxyState::Unknown
    ));
}

#[tokio::test]
async fn answering_server_is_reachable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/health"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"{\"status\":\"ok\"}", "application/json"),
        )
        .mount(&server)
        .await;

    let report = Inspector::new(parse(&server.uri()))
        .check()
        .await
        .expect("check must complete");

    assert_eq!(report.endpoint, EndpointState::Reachable);
    assert_known_proxy_state(report.proxy);
}

#[tokio::test]
async fn reverse_proxy_prefix_is_kept() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/exams/api/health"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let report = Inspector::new(parse(&format!("{}/exams", server.uri())))
        .check()
        .await
        .expect("check must complete");

    assert_eq!(report.endpoint, EndpointState::Reachable);
}

#[tokio::test]
async fn failing_status_still_means_the_server_answered() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/health"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let report = Inspector::new(parse(&server.uri()))
        .check()
        .await
        .expect("check must complete");

    assert_eq!(report.endpoint, EndpointState::Reachable);
}

#[tokio::test]
async fn closed_port_is_unreachable() {
    // Binding and dropping a listener leaves a port that nothing listens on, so
    // the connection is refused instead of timing out.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener must bind");
    let address = listener
        .local_addr()
        .expect("listener must have an address");
    drop(listener);

    let report = Inspector::new(parse(&format!("http://{address}")))
        .check()
        .await
        .expect("check must complete");

    assert_eq!(report.endpoint, EndpointState::Unreachable);
    assert_known_proxy_state(report.proxy);
}

#[tokio::test]
async fn non_http_origin_is_unreachable() {
    let report = Inspector::new(parse("file:///tmp/health"))
        .check()
        .await
        .expect("check must complete");

    assert_eq!(report.endpoint, EndpointState::Unreachable);
}
