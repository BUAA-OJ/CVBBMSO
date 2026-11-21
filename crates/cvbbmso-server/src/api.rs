use std::{borrow::Cow, sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    extract::{
        MatchedPath, Path, Request, State, WebSocketUpgrade, rejection::PathRejection,
        ws::rejection::WebSocketUpgradeRejection,
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use cvbbmso_accoding::{AccodingCookies, AccodingError, Verifier};
use cvbbmso_domain::{DomainError, ExamService};
use cvbbmso_protocol::{AccodingUser, ExamListResponse, error_code};
use serde_json::json;
use tracing::{Instrument, Span};
use uuid::Uuid;

use crate::{
    hub::{Hub, StudentKey},
    socket,
};

#[derive(Clone)]
pub(crate) struct AppState {
    exam: Arc<ExamService>,
    verifier: Arc<IdentityVerifier>,
    hub: Hub,
    timeout: Duration,
}

#[cfg(test)]
type VerifyFn = dyn Fn(&AccodingCookies) -> Result<AccodingUser, AccodingError> + Send + Sync;

#[derive(Clone, Copy)]
struct RequestId(Uuid);

enum IdentityVerifier {
    Accoding(Verifier),
    #[cfg(test)]
    Test(Box<VerifyFn>),
}

impl AppState {
    pub(crate) fn new(
        exam: Arc<ExamService>,
        verifier: Verifier,
        hub: Hub,
        timeout: Duration,
    ) -> Self {
        Self {
            exam,
            verifier: Arc::new(IdentityVerifier::Accoding(verifier)),
            hub,
            timeout,
        }
    }

    async fn authenticate(&self, headers: &HeaderMap) -> Result<AccodingUser, HttpError> {
        let cookies = parse_cookies(headers).map_err(|error| HttpError::authentication(&error))?;
        let user = match self.verifier.as_ref() {
            IdentityVerifier::Accoding(verifier) => verifier.verify(&cookies).await,
            #[cfg(test)]
            IdentityVerifier::Test(verify) => verify(&cookies),
        }
        .map_err(|error| HttpError::authentication(&error))?;
        Span::current().record("oj_user_id", user.id);
        Ok(user)
    }
}

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/exams", get(exams))
        .route("/api/exams/{exam_id}/ws", get(upgrade))
        .fallback(invalid_route)
        .method_not_allowed_fallback(invalid_route)
        .layer(middleware::from_fn(log_request))
        .with_state(state)
}

async fn log_request(mut request: Request, next: Next) -> Response {
    let request_id = RequestId(Uuid::new_v4());
    request.extensions_mut().insert(request_id);
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str);
    let span = tracing::info_span!("http_request", %route, request_id = %request_id.0, method = %request.method(), exam_id = tracing::field::Empty, oj_user_id = tracing::field::Empty);
    async {
        let response = next.run(request).await;
        tracing::info!(status = response.status().as_u16(), "request completed");
        response
    }
    .instrument(span)
    .await
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn exams(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ExamListResponse>, HttpError> {
    let user = state.authenticate(&headers).await?;
    let items = state
        .exam
        .list_for(user.id)
        .await
        .map_err(|error| HttpError::domain(&error))?;
    Ok(Json(ExamListResponse { user, items }))
}

async fn upgrade(
    State(state): State<AppState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    path: Result<Path<Uuid>, PathRejection>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let Path(exam_id) = path.map_err(|_| HttpError::invalid_request("invalid exam ID"))?;
    Span::current().record("exam_id", tracing::field::display(exam_id));
    let upgrade = upgrade.map_err(|_| HttpError::invalid_request("invalid WebSocket upgrade"))?;
    validate_upgrade_headers(&headers)?;
    let user = state.authenticate(&headers).await?;
    let duration = state
        .exam
        .check_enrollment(exam_id, user.id)
        .await
        .map_err(|error| HttpError::domain(&error))?;
    let message_limit = socket::message_limit(duration);
    let key = StudentKey {
        exam_id,
        oj_user_id: user.id,
    };
    let request_span = Span::current();
    let failure_span = request_span.clone();
    Ok(upgrade
        .max_frame_size(message_limit)
        .max_message_size(message_limit)
        .on_failed_upgrade(move |_| {
            failure_span.in_scope(|| tracing::warn!("WebSocket upgrade failed"));
        })
        .on_upgrade(move |ws| {
            socket::run(ws, state.hub, key, request_id, state.timeout).instrument(request_span)
        }))
}

fn validate_upgrade_headers(headers: &HeaderMap) -> Result<(), HttpError> {
    let upgrade = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| std::str::from_utf8(value.as_bytes()).ok())
        .flat_map(|value| value.split(','))
        .any(|token| {
            token
                .trim_matches([' ', '\t'])
                .eq_ignore_ascii_case("upgrade")
        });
    let mut keys = headers.get_all(header::SEC_WEBSOCKET_KEY).iter();
    let key = keys
        .next()
        .ok_or_else(|| HttpError::invalid_request("missing WebSocket key"))?;
    let mut nonce = [0; 16];
    if !upgrade
        || keys.next().is_some()
        || STANDARD.decode_slice(key, &mut nonce) != Ok(nonce.len())
    {
        return Err(HttpError::invalid_request(
            "invalid WebSocket handshake headers",
        ));
    }
    Ok(())
}

fn parse_cookies(headers: &HeaderMap) -> Result<AccodingCookies, AccodingError> {
    let mut pairs = Vec::new();
    for header in headers.get_all(header::COOKIE) {
        // HeaderValue::to_str rejects tabs, which are valid delimiters here.
        let header = std::str::from_utf8(header.as_bytes())
            .map_err(|_| AccodingError::InvalidHeader(None))?;
        for pair in header.split(';') {
            let (name, value) = pair
                .trim_matches([' ', '\t'])
                .split_once('=')
                .ok_or(AccodingError::InvalidHeader(None))?;
            pairs.push((name.to_owned(), value.to_owned()));
        }
    }
    AccodingCookies::from_pairs(pairs)
}

async fn invalid_route() -> HttpError {
    HttpError::invalid_request("unrecognized route or method")
}

#[derive(Debug)]
struct HttpError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    reason: Cow<'static, str>,
}

impl HttpError {
    const INVALID_REQUEST: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: error_code::INVALID_REQUEST,
        message: "Invalid request",
        reason: Cow::Borrowed("invalid request"),
    };
    const ACCODING_AUTH_EXPIRED: Self = Self {
        status: StatusCode::UNAUTHORIZED,
        code: error_code::ACCODING_AUTH_EXPIRED,
        message: "Accoding login required",
        reason: Cow::Borrowed("Accoding authentication rejected"),
    };
    const NOT_ENROLLED: Self = Self {
        status: StatusCode::FORBIDDEN,
        code: error_code::NOT_ENROLLED,
        message: "Exam enrollment required",
        reason: Cow::Borrowed("user is not enrolled"),
    };
    const EXAM_NOT_FOUND: Self = Self {
        status: StatusCode::NOT_FOUND,
        code: error_code::EXAM_NOT_FOUND,
        message: "Exam not found",
        reason: Cow::Borrowed("exam not found"),
    };
    const ACCODING_UNAVAILABLE: Self = Self {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: error_code::ACCODING_UNAVAILABLE,
        message: "Accoding identity service unavailable",
        reason: Cow::Borrowed("Accoding identity request failed"),
    };
    const INTERNAL_ERROR: Self = Self {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: error_code::INTERNAL_ERROR,
        message: "Internal server error",
        reason: Cow::Borrowed("internal failure"),
    };

    fn invalid_request(reason: &'static str) -> Self {
        Self {
            reason: Cow::Borrowed(reason),
            ..Self::INVALID_REQUEST
        }
    }

    fn authentication(error: &AccodingError) -> Self {
        // Upstream errors can contain URLs or response data.
        let (response, reason) = match error {
            AccodingError::InvalidHeader(_) => {
                return Self::invalid_request("invalid Accoding cookie header");
            }
            AccodingError::MissingSession => (
                Self::ACCODING_AUTH_EXPIRED,
                "missing Accoding session cookie".into(),
            ),
            AccodingError::Unauthenticated => (
                Self::ACCODING_AUTH_EXPIRED,
                "Accoding session rejected".into(),
            ),
            AccodingError::InvalidResponse(Some(error)) => (
                Self::ACCODING_UNAVAILABLE,
                format!("invalid Accoding JSON response ({:?})", error.classify()).into(),
            ),
            AccodingError::InvalidResponse(None) => (
                Self::ACCODING_UNAVAILABLE,
                "unexpected Accoding identity response".into(),
            ),
            AccodingError::Upstream(error) => {
                let reason = if let Some(status) = error.status() {
                    format!("Accoding returned HTTP {}", status.as_u16()).into()
                } else if error.is_timeout() {
                    "Accoding request timed out".into()
                } else if error.is_connect() {
                    "Accoding connection failed".into()
                } else if error.is_body() {
                    "failed to read Accoding response body".into()
                } else {
                    "Accoding HTTP request failed".into()
                };
                (Self::ACCODING_UNAVAILABLE, reason)
            }
            _ => (Self::INTERNAL_ERROR, "Accoding verification failed".into()),
        };
        Self { reason, ..response }
    }

    fn domain(error: &DomainError) -> Self {
        Self {
            reason: error.diagnostic(),
            ..match error {
                DomainError::InvalidInput => Self::INVALID_REQUEST,
                DomainError::NotEnrolled => Self::NOT_ENROLLED,
                DomainError::NotFound => Self::EXAM_NOT_FOUND,
                _ => Self::INTERNAL_ERROR,
            }
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        tracing::warn!(code = self.code, reason = %self.reason, "request rejected");
        (
            self.status,
            Json(json!({ "error": { "code": self.code, "message": self.message } })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests;
