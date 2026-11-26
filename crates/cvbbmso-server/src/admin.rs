use std::{collections::BTreeMap, env, sync::Arc};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State, rejection::JsonRejection},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use cvbbmso_domain::{AdminOperation, AdminOutput, DomainError};
use cvbbmso_protocol::admin::{
    CreateExam, CreatedExam, ErrorResponse, Event, ExamDetail, ImportParticipants,
    ImportedParticipants, Incident, IncidentFilter, RosterEntry, StudentStatus,
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::hub::Hub;

const TOKEN_LENGTH: usize = 64;
const ADMIN_ACTOR: &str = "control_panel";

#[derive(Clone)]
pub(crate) struct Token(Arc<[u8; TOKEN_LENGTH]>);

#[derive(Debug, thiserror::Error)]
#[error("CVBBMSO_ADMIN_TOKEN must be 64 lowercase hexadecimal characters")]
pub(crate) struct InvalidToken;

impl Token {
    pub(crate) fn from_environment() -> Result<Option<Self>, InvalidToken> {
        match env::var("CVBBMSO_ADMIN_TOKEN") {
            Ok(value) => Self::parse(&value).map(Some),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(_)) => Err(InvalidToken),
        }
    }

    fn parse(value: &str) -> Result<Self, InvalidToken> {
        let bytes: [u8; TOKEN_LENGTH] = value.as_bytes().try_into().map_err(|_| InvalidToken)?;
        if !bytes
            .iter()
            .copied()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(InvalidToken);
        }
        Ok(Self(Arc::new(bytes)))
    }

    fn accepts(&self, value: &HeaderValue) -> bool {
        value
            .as_bytes()
            .strip_prefix(b"Bearer ")
            .and_then(|candidate| <&[u8; TOKEN_LENGTH]>::try_from(candidate).ok())
            .is_some_and(|candidate| bool::from(self.0.ct_eq(candidate)))
    }
}

pub(crate) fn router(hub: Hub, token: Token) -> Router {
    let api = Router::new()
        .route("/exams", get(exams).post(create_exam))
        .route("/exams/{exam_id}", get(exam))
        .route("/exams/{exam_id}/roster", get(roster))
        .route("/exams/{exam_id}/participants", axum::routing::post(import))
        .route(
            "/exams/{exam_id}/participants/{oj_user_id}",
            axum::routing::delete(remove),
        )
        .route("/exams/{exam_id}/incidents", get(incidents))
        .route("/exams/{exam_id}/students/{oj_user_id}/events", get(events))
        .route(
            "/exams/{exam_id}/students/{oj_user_id}/recording",
            get(recording),
        )
        .layer(DefaultBodyLimit::max(65_536))
        .route_layer(middleware::from_fn_with_state(token, authorize))
        .with_state(hub);

    Router::new()
        .nest("/api/admin", api)
        .route(
            "/admin",
            get(|| async { Html(include_str!("../ui/index.html")) }),
        )
        .route("/admin/", get(|| async { Redirect::temporary("/admin") }))
        .route(
            "/admin/main.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../ui/main.js"),
                )
            }),
        )
        .route(
            "/admin/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../ui/style.css"),
                )
            }),
        )
        .layer(middleware::from_fn(response_headers))
}

async fn authorize(State(token): State<Token>, request: Request, next: Next) -> Response {
    let mut credentials = request.headers().get_all(header::AUTHORIZATION).iter();
    if credentials.next().is_some_and(|value| token.accepts(value)) && credentials.next().is_none()
    {
        next.run(request).await
    } else {
        let mut response =
            AdminError(StatusCode::UNAUTHORIZED, "ADMIN_AUTH_REQUIRED").into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        response
    }
}

async fn response_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(
        "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
    ));
    response
}

struct AdminError(StatusCode, &'static str);

impl AdminError {
    const INVALID: Self = Self(StatusCode::BAD_REQUEST, "INVALID_INPUT");
    const INTERNAL: Self = Self(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR");

    fn body(error: &JsonRejection) -> Self {
        match error.status() {
            StatusCode::PAYLOAD_TOO_LARGE => Self(StatusCode::PAYLOAD_TOO_LARGE, "BODY_TOO_LARGE"),
            StatusCode::UNSUPPORTED_MEDIA_TYPE => {
                Self(StatusCode::UNSUPPORTED_MEDIA_TYPE, "JSON_REQUIRED")
            }
            _ => Self::INVALID,
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorResponse { code: self.1 })).into_response()
    }
}

impl From<DomainError> for AdminError {
    fn from(error: DomainError) -> Self {
        match error {
            DomainError::InvalidInput => Self::INVALID,
            DomainError::NotFound | DomainError::NotEnrolled => {
                Self(StatusCode::NOT_FOUND, "NOT_FOUND")
            }
            DomainError::Conflict => Self(StatusCode::CONFLICT, "CONFLICT"),
            _ if error.is_storage_busy() => Self(StatusCode::SERVICE_UNAVAILABLE, "STORAGE_BUSY"),
            _ => {
                tracing::error!(reason = %error.diagnostic(), "admin operation failed");
                Self::INTERNAL
            }
        }
    }
}

async fn exams(State(hub): State<Hub>) -> Result<Response, AdminError> {
    execute(&hub, AdminOperation::ExamsList).await
}

async fn exam(State(hub): State<Hub>, Path(exam_id): Path<Uuid>) -> Result<Response, AdminError> {
    execute(&hub, AdminOperation::ExamsShow { exam_id }).await
}

async fn create_exam(
    State(hub): State<Hub>,
    body: Result<Json<CreateExam>, JsonRejection>,
) -> Result<Response, AdminError> {
    let Json(body) = body.map_err(|error| AdminError::body(&error))?;
    let title = body.title.trim();
    if title.is_empty() || title.chars().count() > 120 {
        return Err(AdminError::INVALID);
    }
    execute(
        &hub,
        AdminOperation::ExamsCreate {
            title: title.to_owned(),
            start_at: body.start_at,
            end_at: body.end_at,
            policy: body.policy,
        },
    )
    .await
}

async fn import(
    State(hub): State<Hub>,
    Path(exam_id): Path<Uuid>,
    body: Result<Json<ImportParticipants>, JsonRejection>,
) -> Result<Response, AdminError> {
    let Json(body) = body.map_err(|error| AdminError::body(&error))?;
    if body.oj_user_ids.is_empty() || body.oj_user_ids.len() > 1_000 {
        return Err(AdminError::INVALID);
    }
    let oj_user_ids = body
        .oj_user_ids
        .iter()
        .map(|value| value.parse())
        .collect::<Result<_, _>>()
        .map_err(|_| AdminError::INVALID)?;
    execute(
        &hub,
        AdminOperation::ParticipantsImport {
            exam_id,
            oj_user_ids,
        },
    )
    .await
}

async fn remove(
    State(hub): State<Hub>,
    Path((exam_id, oj_user_id)): Path<(Uuid, i64)>,
) -> Result<Response, AdminError> {
    execute(
        &hub,
        AdminOperation::ParticipantsRemove {
            exam_id,
            oj_user_id,
        },
    )
    .await
}

async fn incidents(
    State(hub): State<Hub>,
    Path(exam_id): Path<Uuid>,
    Query(filter): Query<IncidentFilter>,
) -> Result<Response, AdminError> {
    execute(
        &hub,
        AdminOperation::IncidentsList {
            exam_id,
            oj_user_id: filter.oj_user_id,
            kind: None,
            since: None,
            until: None,
        },
    )
    .await
}

async fn events(
    State(hub): State<Hub>,
    Path((exam_id, oj_user_id)): Path<(Uuid, i64)>,
) -> Result<Response, AdminError> {
    execute(
        &hub,
        AdminOperation::EventsList {
            exam_id,
            oj_user_id,
        },
    )
    .await
}

async fn recording(
    State(hub): State<Hub>,
    Path((exam_id, oj_user_id)): Path<(Uuid, i64)>,
) -> Result<Response, AdminError> {
    execute(
        &hub,
        AdminOperation::RecordingsShow {
            exam_id,
            oj_user_id,
        },
    )
    .await
}

async fn roster(
    State(hub): State<Hub>,
    Path(exam_id): Path<Uuid>,
) -> Result<Json<Vec<RosterEntry>>, AdminError> {
    let AdminOutput::Participants(participants) = hub
        .admin(
            ADMIN_ACTOR.into(),
            AdminOperation::ParticipantsList { exam_id },
        )
        .await?
    else {
        return Err(AdminError::INTERNAL);
    };
    let AdminOutput::Students(students) = hub
        .admin(
            ADMIN_ACTOR.into(),
            AdminOperation::StudentsStatus { exam_id },
        )
        .await?
    else {
        return Err(AdminError::INTERNAL);
    };
    let mut entries = BTreeMap::new();
    for participant in participants {
        entries.insert(
            participant.oj_user_id,
            RosterEntry {
                oj_user_id: participant.oj_user_id.to_string(),
                enrolled: true,
                runtime: None,
            },
        );
    }
    for student in students {
        entries
            .entry(student.oj_user_id)
            .or_insert_with(|| RosterEntry {
                oj_user_id: student.oj_user_id.to_string(),
                enrolled: false,
                runtime: None,
            })
            .runtime = Some(StudentStatus {
            connected: student.connected,
            execution: student.execution,
            last_seen_at: student.last_seen_at,
        });
    }
    Ok(Json(entries.into_values().collect()))
}

async fn execute(hub: &Hub, operation: AdminOperation) -> Result<Response, AdminError> {
    let output = hub.admin(ADMIN_ACTOR.into(), operation).await?;
    Ok(match output {
        AdminOutput::ExamCreated { exam_id } => {
            (StatusCode::CREATED, Json(CreatedExam { exam_id })).into_response()
        }
        AdminOutput::Exams(items) => Json(items).into_response(),
        AdminOutput::ExamDetail { summary, policy } => {
            Json(ExamDetail { summary, policy }).into_response()
        }
        AdminOutput::ParticipantsImported { count } => {
            Json(ImportedParticipants { count }).into_response()
        }
        AdminOutput::ParticipantRemoved => StatusCode::NO_CONTENT.into_response(),
        AdminOutput::Incidents(items) => Json(
            items
                .into_iter()
                .map(|item| Incident {
                    id: item.id,
                    oj_user_id: item.oj_user_id.to_string(),
                    source: item.source,
                    kind: item.kind,
                    occurred_at: item.occurred_at,
                    detail: item.detail,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        AdminOutput::Events(items) => Json(
            items
                .into_iter()
                .map(|item| Event {
                    seq: item.seq.to_string(),
                    event: item.event,
                    received_at: item.received_at,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        AdminOutput::Recording(summary) => Json(summary).into_response(),
        _ => return Err(AdminError::INTERNAL),
    })
}

#[cfg(test)]
mod tests;
