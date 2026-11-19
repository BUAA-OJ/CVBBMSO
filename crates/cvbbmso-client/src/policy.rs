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
