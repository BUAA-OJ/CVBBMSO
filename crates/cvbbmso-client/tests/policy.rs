//! Regressions for parsed origins and identity cookie scope.

#[path = "../src/desktop/macos/cookie_scope.rs"]
mod cookie_scope;
#[path = "../src/policy.rs"]
mod policy;

use cookie_scope::matches_cookie;
use policy::{IDENTITY_URL, LOGIN_URL, permits, server_origin};
use url::Url;

#[test]
fn oj_navigation_requires_the_parsed_https_origin() {
    let origin = Url::parse(LOGIN_URL).unwrap().origin();
    assert_eq!(Url::parse(IDENTITY_URL).unwrap().origin(), origin);
    for allowed in [LOGIN_URL, "https://ACCODING.buaa.edu.cn:443/problem/42"] {
        assert!(permits(&origin, &Url::parse(allowed).unwrap()));
    }
    for denied in [
        "http://accoding.buaa.edu.cn/user/login",
        "https://accoding.buaa.edu.cn:444/",
        "https://accoding.buaa.edu.cn.example.org/",
        "https://accoding.buaa.edu.cn@example.org/",
        "https://user@accoding.buaa.edu.cn/",
        "blob:https://accoding.buaa.edu.cn/id",
        "mailto:user@example.org",
        "tauri://localhost/",
    ] {
        assert!(!permits(&origin, &Url::parse(denied).unwrap()), "{denied}");
    }
}

#[test]
fn monitor_configuration_accepts_only_an_https_origin() {
    let origin = server_origin("https://monitor.example.org:8443/").unwrap();
    assert_eq!(
        origin.ascii_serialization(),
        "https://monitor.example.org:8443"
    );
    for invalid in [
        "",
        "http://monitor.example.org",
        "https://monitor.example.org/path",
        "https://user@monitor.example.org/",
        "https://monitor.example.org/?token=secret",
        "https://monitor.example.org/#anchor",
    ] {
        assert!(server_origin(invalid).is_none(), "{invalid}");
    }
}

#[test]
fn native_cookies_must_match_the_identity_host_and_path() {
    let url = Url::parse(IDENTITY_URL).unwrap();
    let host = url.host_str();
    assert!(matches_cookie(&url, host, Some("/"), true));
    assert!(matches_cookie(
        &url,
        Some(".buaa.edu.cn"),
        Some("/api/"),
        true
    ));
    assert!(!matches_cookie(&url, Some("buaa.edu.cn"), Some("/"), true));
    assert!(!matches_cookie(&url, Some(".uaa.edu.cn"), Some("/"), true));
    assert!(!matches_cookie(&url, host, Some("/api/user"), true));
    assert!(!matches_cookie(&url, host, Some("/user"), false));
}
