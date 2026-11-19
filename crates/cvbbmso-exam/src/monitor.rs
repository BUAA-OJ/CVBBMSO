//! Endpoints of the monitor server used by the runtime.
//!
//! The runtime checks enrollment over HTTP before it opens the WebSocket, so
//! the candidate directory can be located from the authenticated user id. Both
//! requests carry the native Accoding cookies; neither follows redirects or a
//! system proxy.

use std::time::Duration;

use cvbbmso_accoding::AccodingCookies;
use cvbbmso_protocol::{ExamListResponse, ExamSummary};
use reqwest::{Client, Response, StatusCode, header, redirect};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, handshake::client::Request};
use url::Url;
use uuid::Uuid;

use crate::ExamError;

/// Largest exam list body accepted, matching the desktop client.
const MAX_EXAM_LIST_BYTES: usize = 1024 * 1024;
/// Upper bound of one HTTP request, including the body.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// HTTP client and endpoint addresses of one monitor server.
pub(crate) struct Monitor {
    base: Url,
    http: Client,
}

/// Enrollment confirmed by the monitor for one exam.
#[derive(Debug)]
pub(crate) struct Enrollment {
    /// Accoding user id of the authenticated student.
    pub(crate) oj_user_id: i64,
    /// Summary of the requested exam.
    pub(crate) exam: ExamSummary,
}

impl Monitor {
    /// Creates a client for the monitor at `base`.
    ///
    /// `base` must be an `http` or `https` URL with a host; the public entry
    /// point additionally requires `https`.
    pub(crate) fn new(base: Url) -> Result<Self, ExamError> {
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(ExamError::Http(None));
        }
        let http = Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| ExamError::Http(Some(error)))?;
        Ok(Self { base, http })
    }

    /// Confirms that the cookie owner is enrolled in `exam_id`.
    ///
    /// # Errors
    ///
    /// [`ExamError::Auth`] for a missing or expired session,
    /// [`ExamError::NotEnrolled`] when the exam is absent from the student's
    /// list, and [`ExamError::Http`] for transport failures, other statuses,
    /// and malformed or oversized bodies.
    pub(crate) async fn enrollment(
        &self,
        cookies: &AccodingCookies,
        exam_id: Uuid,
    ) -> Result<Enrollment, ExamError> {
        let url = self
            .base
            .join("/api/exams")
            .map_err(|_| ExamError::Http(None))?;
        let response = self
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
            StatusCode::FORBIDDEN => return Err(ExamError::NotEnrolled),
            _ => return Err(ExamError::Http(None)),
        }
        let list = read_exam_list(response).await?;
        let exam = list
            .items
            .into_iter()
            .find(|exam| exam.id == exam_id)
            .ok_or(ExamError::NotEnrolled)?;
        Ok(Enrollment {
            oj_user_id: list.user.id,
            exam,
        })
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

async fn read_exam_list(mut response: Response) -> Result<ExamListResponse, ExamError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ExamError::Http(Some(error)))?
    {
        if chunk.len() > MAX_EXAM_LIST_BYTES - body.len() {
            return Err(ExamError::Http(None));
        }
        body.extend_from_slice(&chunk);
    }
    // Serde's struct decoder also accepts positional arrays, which the HTTP
    // contract excludes.
    if body.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(ExamError::Http(None));
    }
    serde_json::from_slice(&body).map_err(|_| ExamError::Http(None))
}

#[cfg(test)]
mod tests;
