use http::HeaderValue;

use crate::AccodingError;

const SESSION_COOKIE: &str = "connect.sid";

/// A validated, sensitive `Cookie` header for an Accoding session.
#[derive(Clone, Debug)]
pub struct AccodingCookies(HeaderValue);

impl AccodingCookies {
    /// Builds a header from `WebView` cookies or already parsed request cookies.
    /// Cookie values and pair order are preserved.
    ///
    /// # Errors
    ///
    /// Returns [`AccodingError::MissingSession`] for a missing or empty session cookie,
    /// or [`AccodingError::InvalidHeader`] for invalid pairs or duplicate session cookies.
    pub fn from_pairs(
        pairs: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, AccodingError> {
        let mut session_length = None;
        let mut encoded = String::new();

        for (name, value) in pairs {
            let contents = value
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
                .unwrap_or(&value);
            if !valid_name(&name) || !contents.bytes().all(is_cookie_octet) {
                return Err(AccodingError::InvalidHeader(None));
            }
            if name == SESSION_COOKIE && session_length.replace(contents.len()).is_some() {
                return Err(AccodingError::InvalidHeader(None));
            }
            if !encoded.is_empty() {
                encoded.push_str("; ");
            }
            encoded.push_str(&name);
            encoded.push('=');
            encoded.push_str(&value);
        }

        if session_length.unwrap_or_default() == 0 {
            return Err(AccodingError::MissingSession);
        }
        let mut header = HeaderValue::try_from(encoded)
            .map_err(|source| AccodingError::InvalidHeader(Some(source)))?;
        header.set_sensitive(true);
        Ok(Self(header))
    }

    /// Returns the encoded header, marked sensitive for HTTP and WebSocket callers.
    #[must_use]
    pub fn header_value(&self) -> HeaderValue {
        self.0.clone()
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn is_cookie_octet(byte: u8) -> bool {
    // RFC 6265 excludes whitespace, quotes, commas, semicolons and backslashes.
    matches!(byte, b'!' | b'#'..=b'+' | b'-'..=b':' | b'<'..=b'[' | b']'..=b'~')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookies<const N: usize>(pairs: [(&str, &str); N]) -> Result<AccodingCookies, AccodingError> {
        AccodingCookies::from_pairs(pairs.map(|(name, value)| (name.into(), value.into())))
    }

    #[test]
    fn preserves_encoded_and_auxiliary_cookies_in_order() {
        let cookies = cookies([
            ("_zte_cid_", ""),
            ("connect.sid", "s%3Asynthetic.sig%2B%3D"),
            ("_zte_sid_", "\"auxiliary=\""),
        ])
        .unwrap();
        let header = cookies.header_value();
        assert_eq!(
            header,
            "_zte_cid_=; connect.sid=s%3Asynthetic.sig%2B%3D; _zte_sid_=\"auxiliary=\""
        );
        assert!(header.is_sensitive());
        assert!(!format!("{cookies:?}").contains("synthetic"));
    }

    #[test]
    fn requires_a_nonempty_case_sensitive_session_cookie() {
        for pairs in [
            [("_zte_sid_", "synthetic")],
            [("Connect.sid", "synthetic")],
            [("connect.sid", "")],
            [("connect.sid", "\"\"")],
        ] {
            assert!(matches!(cookies(pairs), Err(AccodingError::MissingSession)));
        }
        assert!(matches!(cookies([]), Err(AccodingError::MissingSession)));
    }

    #[test]
    fn rejects_duplicate_session_cookies() {
        for first_value in ["", "old"] {
            assert!(matches!(
                cookies([("connect.sid", first_value), ("connect.sid", "new")]),
                Err(AccodingError::InvalidHeader(_))
            ));
        }
    }

    #[test]
    fn rejects_invalid_cookie_pairs() {
        for pair in [
            ("", "value"),
            ("other=name", "value"),
            ("other name", "value"),
            ("other", "a; extra=value"),
            ("other", "a\r\nX-Test: value"),
            ("other", "a b"),
            ("other", "a,b"),
            ("other", "\"unpaired"),
            ("other", "a\\b"),
            ("other", "你好"),
        ] {
            assert!(matches!(
                cookies([("connect.sid", "synthetic"), pair]),
                Err(AccodingError::InvalidHeader(_))
            ));
        }
    }
}
