use std::{sync::Arc, time::Duration};

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
    let Path(exam_id) = path.map_err(|_| HttpError::INVALID_REQUEST)?;
    Span::current().record("exam_id", tracing::field::display(exam_id));
    let upgrade = upgrade.map_err(|_| HttpError::INVALID_REQUEST)?;
    validate_upgrade_headers(&headers)?;
    let user = state.authenticate(&headers).await?;
    state
        .exam
        .check_enrollment(exam_id, user.id)
        .await
        .map_err(|error| HttpError::domain(&error))?;
    let key = StudentKey {
        exam_id,
        oj_user_id: user.id,
    };
    let request_span = Span::current();
    let failure_span = request_span.clone();
    Ok(upgrade
        .max_frame_size(socket::MAX_MESSAGE_BYTES)
        .max_message_size(socket::MAX_MESSAGE_BYTES)
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
    let key = keys.next().ok_or(HttpError::INVALID_REQUEST)?;
    let mut nonce = [0; 16];
    if !upgrade
        || keys.next().is_some()
        || STANDARD.decode_slice(key, &mut nonce) != Ok(nonce.len())
    {
        return Err(HttpError::INVALID_REQUEST);
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
    HttpError::INVALID_REQUEST
}

#[derive(Debug)]
struct HttpError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl HttpError {
    const INVALID_REQUEST: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: error_code::INVALID_REQUEST,
        message: "Invalid request",
    };
    const ACCODING_AUTH_EXPIRED: Self = Self {
        status: StatusCode::UNAUTHORIZED,
        code: error_code::ACCODING_AUTH_EXPIRED,
        message: "Accoding login required",
    };
    const NOT_ENROLLED: Self = Self {
        status: StatusCode::FORBIDDEN,
        code: error_code::NOT_ENROLLED,
        message: "Exam enrollment required",
    };
    const EXAM_NOT_FOUND: Self = Self {
        status: StatusCode::NOT_FOUND,
        code: error_code::EXAM_NOT_FOUND,
        message: "Exam not found",
    };
    const ACCODING_UNAVAILABLE: Self = Self {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: error_code::ACCODING_UNAVAILABLE,
        message: "Accoding identity service unavailable",
    };
    const INTERNAL_ERROR: Self = Self {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: error_code::INTERNAL_ERROR,
        message: "Internal server error",
    };

    fn authentication(error: &AccodingError) -> Self {
        match error {
            AccodingError::InvalidHeader(_) => Self::INVALID_REQUEST,
            AccodingError::MissingSession | AccodingError::Unauthenticated => {
                Self::ACCODING_AUTH_EXPIRED
            }
            AccodingError::Upstream(_) | AccodingError::InvalidResponse(_) => {
                Self::ACCODING_UNAVAILABLE
            }
            _ => Self::INTERNAL_ERROR,
        }
    }

    fn domain(error: &DomainError) -> Self {
        match error {
            DomainError::InvalidInput => Self::INVALID_REQUEST,
            DomainError::NotEnrolled => Self::NOT_ENROLLED,
            DomainError::NotFound => Self::EXAM_NOT_FOUND,
            _ => Self::INTERNAL_ERROR,
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        tracing::warn!(code = self.code, "request rejected");
        (
            self.status,
            Json(json!({ "error": { "code": self.code, "message": self.message } })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests;
