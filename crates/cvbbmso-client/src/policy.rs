#[cfg(any(target_os = "macos", test))]
use std::net::IpAddr;

use url::{Origin, Url};

pub const LOGIN_URL: &str = "https://accoding.buaa.edu.cn/user/login";
pub const IDENTITY_URL: &str = "https://accoding.buaa.edu.cn/api/users/me";

pub fn server_origin(value: &str) -> Option<Origin> {
    let url = Url::parse(value).ok()?;
    (url.scheme() == "https"
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.origin())
}

pub fn permits(origin: &Origin, url: &Url) -> bool {
    matches!(origin, Origin::Tuple(scheme, ..) if scheme == url.scheme())
        && &url.origin() == origin
        && url.username().is_empty()
        && url.password().is_none()
}

#[cfg(any(target_os = "macos", test))]
pub fn matches_cookie(url: &Url, domain: Option<&str>, path: Option<&str>, secure: bool) -> bool {
    let (Some(host), Some(domain), Some(path)) = (url.host_str(), domain, path) else {
        return false;
    };
    // WebKit prefixes Domain cookies with a dot, retaining the distinction
    // between host-only and domain cookies when listing the native store.
    let domain_matches = domain.strip_prefix('.').map_or_else(
        || host.eq_ignore_ascii_case(domain),
        |domain| {
            host.eq_ignore_ascii_case(domain)
                || (host.parse::<IpAddr>().is_err()
                    && host.len() > domain.len()
                    && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
                    && host.as_bytes()[host.len() - domain.len()..]
                        .eq_ignore_ascii_case(domain.as_bytes()))
        },
    );
    let path_matches = url.path() == path
        || url
            .path()
            .strip_prefix(path)
            .is_some_and(|tail| path.ends_with('/') || tail.starts_with('/'));
    domain_matches && path.starts_with('/') && path_matches && (!secure || url.scheme() == "https")
}
