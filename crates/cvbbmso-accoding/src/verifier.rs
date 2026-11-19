use std::time::Duration;

use cvbbmso_protocol::AccodingUser;
use reqwest::{Client, Response, StatusCode, header, redirect};

use crate::{AccodingCookies, AccodingError};

const IDENTITY_URL: &str = "https://accoding.buaa.edu.cn/api/users/me";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Verifies an account against the fixed Accoding student identity endpoint.
pub struct Verifier {
    http: Client,
}

impl Verifier {
    /// Creates a reusable HTTP client with a ten-second request timeout and no redirects.
    ///
    /// # Errors
    ///
    /// Returns [`AccodingError::Upstream`] if the HTTP client cannot be initialized.
    pub fn new() -> Result<Self, AccodingError> {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // Reuse the client without giving a caller control over where cookies can go.
            .redirect(redirect::Policy::none())
            .build()
            .map_err(AccodingError::Upstream)?;
        Ok(Self { http })
    }

    /// Fetches the current account using the supplied student session cookies.
    ///
    /// # Errors
    ///
    /// Returns [`AccodingError::Unauthenticated`] for anonymous or expired sessions,
    /// [`AccodingError::Upstream`] for request failures, or
    /// [`AccodingError::InvalidResponse`] for unexpected statuses or invalid identities.
    pub async fn verify(&self, cookies: &AccodingCookies) -> Result<AccodingUser, AccodingError> {
        self.verify_at(IDENTITY_URL, cookies).await
    }

    async fn verify_at(
        &self,
        url: &str,
        cookies: &AccodingCookies,
    ) -> Result<AccodingUser, AccodingError> {
        let response = self
            .http
            .get(url)
            .header(header::ACCEPT, "application/json")
            .header(header::COOKIE, cookies.header_value())
            .send()
            .await
            .map_err(AccodingError::Upstream)?;

        if response.status() == StatusCode::UNAUTHORIZED || redirects_to_login(&response) {
            return Err(AccodingError::Unauthenticated);
        }
        let response = response
            .error_for_status()
            .map_err(AccodingError::Upstream)?;
        if response.status() != StatusCode::OK {
            return Err(AccodingError::InvalidResponse(None));
        }

        let body = response.bytes().await.map_err(AccodingError::Upstream)?;
        match body.iter().find(|byte| !byte.is_ascii_whitespace()) {
            // Accoding uses an empty HTTP 200 response for anonymous users.
            None => Err(AccodingError::Unauthenticated),
            // Serde structs can deserialize from positional arrays as well as objects.
            Some(b'{') => serde_json::from_slice(&body)
                .map_err(|source| AccodingError::InvalidResponse(Some(source))),
            Some(_) => Err(AccodingError::InvalidResponse(None)),
        }
    }
}

fn redirects_to_login(response: &Response) -> bool {
    response.status().is_redirection()
        && response
            .headers()
            .get(header::LOCATION)
            .and_then(|location| location.to_str().ok())
            .and_then(|location| response.url().join(location).ok())
            .is_some_and(|url| {
                url.origin() == response.url().origin() && url.path() == "/user/login"
            })
}

#[cfg(test)]
mod tests;
