use std::net::IpAddr;

use url::Url;

pub(crate) fn matches_cookie(
    url: &Url,
    domain: Option<&str>,
    path: Option<&str>,
    secure: bool,
) -> bool {
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
