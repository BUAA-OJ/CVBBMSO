//! Monitor endpoints used by the desktop client and exam runtime.
//!
//! The runtime checks enrollment over HTTP before it opens the WebSocket, so
//! the candidate directory can be located from the authenticated user id. Both
//! requests carry the native Accoding cookies; neither follows redirects or a
//! system proxy.

use std::time::Duration;

use cvbbmso_accoding::AccodingCookies;
use cvbbmso_protocol::{ExamListResponse, ExamPhase};
use reqwest::{Certificate, Client, StatusCode, header, redirect};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, handshake::client::Request};
use url::Url;
use uuid::Uuid;

use crate::ExamError;

/// Largest exam list body accepted.
const MAX_EXAM_LIST_BYTES: usize = 1024 * 1024;
/// Upper bound of one HTTP request, including the body.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Connection settings and exam list client for one monitor origin.
#[derive(Debug, Clone)]
pub struct Monitor {
    base: Url,
    http: Client,
}

impl Monitor {
    /// Creates a client for an HTTPS origin without a path, query, fragment, or credentials.
    /// An optional certificate extends the system trust roots for HTTP requests.
    ///
    /// # Errors
    ///
    /// Returns [`ExamError::Http`] if the origin is invalid or the client cannot be built.
    pub fn new(base: Url, root_certificate: Option<Certificate>) -> Result<Self, ExamError> {
        if base.scheme() != "https" {
            return Err(ExamError::Http(None));
        }
        Self::build(base, root_certificate)
    }

    #[cfg(test)]
    pub(crate) fn for_test(base: Url) -> Result<Self, ExamError> {
        Self::build(base, None)
    }

    fn build(base: Url, root_certificate: Option<Certificate>) -> Result<Self, ExamError> {
        if base.host_str().is_none()
            || base.path() != "/"
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.username().is_empty()
            || base.password().is_some()
        {
            return Err(ExamError::Http(None));
        }
        let mut builder = Client::builder()
            .https_only(base.scheme() == "https")
            .no_proxy()
            .redirect(redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT);
        if let Some(certificate) = root_certificate {
            builder = builder.add_root_certificate(certificate);
        }
        let http = builder
            .build()
            .map_err(|error| ExamError::Http(Some(error)))?;
        Ok(Self { base, http })
    }

    /// Returns the HTTPS origin configured for this monitor.
    #[must_use]
    pub fn server_base(&self) -> &Url {
        &self.base
    }

    /// Fetches the authenticated candidate and their current exam list using the given cookies.
    ///
    /// The request has a ten-second deadline, ignores proxies and redirects, and accepts at most
    /// 1 MiB of response body. A successful body must be a JSON object.
    ///
    /// # Errors
    ///
    /// Returns [`ExamError::Auth`] for HTTP 401, [`ExamError::HttpStatus`] for other
    /// statuses except 200, [`ExamError::Http`] for transport failures, and
    /// [`ExamError::InvalidExamList`] for oversized or invalid response bodies.
    pub async fn list_exams(
        &self,
        cookies: &AccodingCookies,
    ) -> Result<ExamListResponse, ExamError> {
        let mut url = self.base.clone();
        url.set_path("/api/exams");
        let mut response = self
            .http
            .get(url)
            .header(header::ACCEPT, "application/json")
            .header(header::COOKIE, cookies.header_value())
            .send()
            .await
            .map_err(|error| ExamError::Http(Some(error)))?;
        match response.status() {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED => return Err(ExamError::Auth),
            status => return Err(ExamError::HttpStatus(status)),
        }

        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| ExamError::Http(Some(error)))?
        {
            if chunk.len() > MAX_EXAM_LIST_BYTES - body.len() {
                return Err(ExamError::InvalidExamList(None));
            }
            body.extend_from_slice(&chunk);
        }
        // Serde's struct decoder also accepts positional arrays, which the HTTP contract excludes.
        if body.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            return Err(ExamError::InvalidExamList(None));
        }
        serde_json::from_slice(&body).map_err(|error| ExamError::InvalidExamList(Some(error)))
    }

    /// Confirms enrollment and returns the authenticated Accoding user id and exam phase.
    pub(crate) async fn enrollment(
        &self,
        cookies: &AccodingCookies,
        exam_id: Uuid,
    ) -> Result<(i64, ExamPhase), ExamError> {
        let list = self
            .list_exams(cookies)
            .await
            .map_err(|error| match error {
                ExamError::HttpStatus(StatusCode::FORBIDDEN) => ExamError::NotEnrolled,
                ExamError::HttpStatus(_) | ExamError::InvalidExamList(_) => ExamError::Http(None),
                error => error,
            })?;
        let exam = list
            .items
            .iter()
            .find(|exam| exam.id == exam_id)
            .ok_or(ExamError::NotEnrolled)?;
        Ok((list.user.id, exam.phase))
    }

    /// Builds the WebSocket handshake request for `exam_id`.
    pub(crate) fn socket_request(
        &self,
        cookies: &AccodingCookies,
        exam_id: Uuid,
    ) -> Result<Request, ExamError> {
        let mut url = self
            .base
            .join(&format!("/api/exams/{exam_id}/ws"))
            .map_err(|_| ExamError::WebSocket(None))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|()| ExamError::WebSocket(None))?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|error| ExamError::WebSocket(Some(error)))?;
        request
            .headers_mut()
            .insert(header::COOKIE, cookies.header_value());
        Ok(request)
    }
}

/// Maps a rejected WebSocket handshake to the runtime error category.
pub(crate) fn handshake_error(error: tungstenite::Error) -> ExamError {
    match &error {
        tungstenite::Error::Http(response) => match response.status() {
            StatusCode::UNAUTHORIZED => ExamError::Auth,
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => ExamError::NotEnrolled,
            _ => ExamError::WebSocket(Some(error)),
        },
        _ => ExamError::WebSocket(Some(error)),
    }
}

#[cfg(test)]
mod tests;
