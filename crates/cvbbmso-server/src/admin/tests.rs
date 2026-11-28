use std::time::Duration;

use axum::{
    body::{Body, to_bytes},
    http::{Method, Request},
};
use cvbbmso_domain::{DisconnectReason, ExamService};
use cvbbmso_protocol::{
    AccodingUser, ClientHello, ClientIncident, ClientMessage, ClientState, ExamEvent,
    ExecutionState, IncidentCode, Platform, RecordingState,
};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tower::ServiceExt;

use super::*;
use crate::hub::Manager;

const TEST_ADMIN_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const LARGE_ID: i64 = i64::MAX;

fn now() -> OffsetDateTime {
    OffsetDateTime::parse("2033-01-01T00:00:00Z", &Rfc3339).unwrap()
}

struct Fixture {
    app: Router,
    exam: Arc<ExamService>,
    manager: tokio::task::JoinHandle<()>,
    _files: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.manager.abort();
    }
}

impl Fixture {
    async fn new() -> Self {
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("admin.sqlite");
        let exam = Arc::new(
            ExamService::open_with_clock(&format!("sqlite://{}?mode=rwc", path.display()), now)
                .await
                .unwrap(),
        );
        let (hub, manager) = Manager::new(Arc::clone(&exam), Duration::from_secs(25));
        let manager = tokio::spawn(manager.run());
        Self {
            app: router(hub, Token::parse(TEST_ADMIN_TOKEN).unwrap()),
            exam,
            manager,
            _files: files,
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        data: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/api/admin{path}"))
            .header(header::AUTHORIZATION, format!("Bearer {TEST_ADMIN_TOKEN}"));
        let body = data.map_or_else(Body::empty, |value| Body::from(value.to_string()));
        request = request.header(header::CONTENT_TYPE, "application/json");
        let response = self
            .app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    async fn create(&self) -> Uuid {
        let (status, body) = self.request(Method::POST, "/exams", Some(json!({
            "title": "Final exam", "start_at": "2033-01-01T09:00:00+08:00",
            "end_at": "2033-01-01T10:00:00+08:00", "policy": { "segment_duration_secs": 30, "fps": 10 }
        }))).await;
        assert_eq!(status, StatusCode::CREATED);
        body["exam_id"].as_str().unwrap().parse().unwrap()
    }

    async fn connect(&self, exam_id: Uuid, oj_user_id: i64) {
        self.exam
            .connect(
                exam_id,
                oj_user_id,
                ClientHello {
                    device_id: Uuid::new_v4(),
                    instance_id: Uuid::new_v4(),
                    platform: Platform::Windows,
                    state: ClientState {
                        execution: ExecutionState::Ready,
                        recording: RecordingState::Idle,
                    },
                },
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn admin_routes_require_bearer_before_reading_or_mutating() {
    let fixture = Fixture::new().await;
    assert!(Token::parse("short").is_err());
    assert!(Token::parse(&"A".repeat(TOKEN_LENGTH)).is_err());
    for authorization in [
        None,
        Some("Bearer bad".into()),
        Some(format!("Bearer {}", "b".repeat(TOKEN_LENGTH))),
    ] {
        for method in [Method::GET, Method::POST] {
            let mut request = Request::builder()
                .method(method)
                .uri("/api/admin/exams")
                .header(header::COOKIE, "connect.sid=student-session");
            if let Some(value) = &authorization {
                request = request.header(header::AUTHORIZATION, value);
            }
            let response = fixture
                .app
                .clone()
                .oneshot(request.body(Body::from("not JSON")).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
    }
    assert_eq!(
        fixture.request(Method::GET, "/exams", None).await,
        (StatusCode::OK, json!([]))
    );
    let page = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(
        page.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    let html = to_bytes(page.into_body(), 16_384).await.unwrap();
    let html = String::from_utf8_lossy(&html);
    assert!(html.contains("id=\"refresh-dashboard\""));
    assert!(!html.contains(TEST_ADMIN_TOKEN));
}

#[tokio::test]
async fn trailing_slash_redirects_to_the_dashboard() {
    let fixture = Fixture::new().await;
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(response.headers()[header::LOCATION], "/admin");
}

#[tokio::test]
async fn school_roster_import_deduplicates_and_preserves_leading_zeros() {
    let fixture = Fixture::new().await;
    let exam_id = fixture.create().await;
    let path = format!("/exams/{exam_id}/student-enrollments");
    assert_eq!(
        fixture
            .request(
                Method::POST,
                &path,
                Some(json!({"student_numbers": ["0007", "7", "0007"]}))
            )
            .await,
        (StatusCode::OK, json!({"count": 2}))
    );
    assert_eq!(
        fixture
            .request(
                Method::POST,
                &path,
                Some(json!({"student_numbers": ["0007"]}))
            )
            .await,
        (StatusCode::OK, json!({"count": 0}))
    );
    for bad in [
        json!([17]),
        json!(["17@buaa.edu.cn"]),
        json!(["１２"]),
        json!([]),
    ] {
        let (status, _) = fixture
            .request(Method::POST, &path, Some(json!({"student_numbers": bad})))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        fixture.request(Method::GET, &path, None).await,
        (
            StatusCode::OK,
            json!([
                {"student_number":"0007","oj_user_id":null,"enrolled":true},
                {"student_number":"7","oj_user_id":null,"enrolled":true}
            ])
        )
    );
    assert_eq!(
        fixture
            .request(Method::DELETE, &format!("{path}/0007"), None)
            .await,
        (StatusCode::NO_CONTENT, Value::Null)
    );
    let (_, roster) = fixture.request(Method::GET, &path, None).await;
    assert_eq!(roster[0]["enrolled"], false);
    assert_eq!(roster[1]["enrolled"], true);
}

#[tokio::test]
async fn labels_historical_evidence_only_on_the_authenticated_identity_route() {
    let fixture = Fixture::new().await;
    let exam_id = fixture.create().await;
    let base = format!("/exams/{exam_id}");
    let identities = format!("{base}/student-identities");
    let id = LARGE_ID.to_string();
    let (status, _) = fixture
        .request(
            Method::POST,
            &format!("{base}/participants"),
            Some(json!({"oj_user_ids": [id]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    fixture.connect(exam_id, LARGE_ID).await;
    let (status, _) = fixture
        .request(Method::DELETE, &format!("{base}/participants/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        fixture.request(Method::GET, &identities, None).await,
        (
            StatusCode::OK,
            json!([{"oj_user_id": id, "student_number": null, "nickname": null}])
        )
    );
    let user = AccodingUser::new(LARGE_ID, "0007@buaa.edu.cn".into(), " 示例考生 ".into()).unwrap();
    fixture
        .exam
        .remember_verified_profile(&user, false)
        .await
        .unwrap();
    assert_eq!(
        fixture.request(Method::GET, &identities, None).await,
        (
            StatusCode::OK,
            json!([{"oj_user_id": id, "student_number": "0007", "nickname": "示例考生"}])
        )
    );
    let anonymous = fixture
        .app
        .clone()
        .oneshot(
            Request::get(format!("/api/admin{identities}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn exam_roster_keeps_never_connected_offline_and_revoked_students_distinct() {
    let fixture = Fixture::new().await;
    let exam_id = fixture.create().await;
    let base = format!("/exams/{exam_id}");
    let (status, detail) = fixture.request(Method::GET, &base, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["summary"]["start_at"], "2033-01-01T01:00:00Z");
    let import = Some(json!({"oj_user_ids": ["12", "13", "14", LARGE_ID.to_string(), "12"]}));
    let import_path = format!("{base}/participants");
    assert_eq!(
        fixture
            .request(Method::POST, &import_path, import.clone())
            .await,
        (StatusCode::OK, json!({"count": 4}))
    );
    assert_eq!(
        fixture.request(Method::POST, &import_path, import).await,
        (StatusCode::OK, json!({"count": 0}))
    );
    let (status, _) = fixture
        .request(
            Method::POST,
            &import_path,
            Some(json!({"oj_user_ids": ["21", "9223372036854775808"]})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    for id in [12, 13, 14] {
        fixture.connect(exam_id, id).await;
    }
    fixture
        .exam
        .disconnect(exam_id, 13, DisconnectReason::Closed)
        .await
        .unwrap();
    let (status, _) = fixture
        .request(Method::DELETE, &format!("{import_path}/14"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, roster) = fixture
        .request(Method::GET, &format!("{base}/roster"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let runtime = |connected| json!({"connected": connected, "execution": "ready", "last_seen_at": "2033-01-01T00:00:00Z"});
    assert_eq!(
        roster,
        json!([
            {"oj_user_id": "12", "enrolled": true, "runtime": runtime(true)},
            {"oj_user_id": "13", "enrolled": true, "runtime": runtime(false)},
            {"oj_user_id": "14", "enrolled": false, "runtime": runtime(false)},
            {"oj_user_id": LARGE_ID.to_string(), "enrolled": true, "runtime": null}
        ])
    );
}

#[tokio::test]
async fn exposes_incident_and_event_evidence_but_no_recording_when_missing() {
    let fixture = Fixture::new().await;
    let exam_id = fixture.create().await;
    let base = format!("/exams/{exam_id}");
    fixture
        .request(
            Method::POST,
            &format!("{base}/participants"),
            Some(json!({"oj_user_ids": ["12", "13"]})),
        )
        .await;
    fixture.connect(exam_id, 12).await;
    fixture
        .exam
        .receive(
            exam_id,
            12,
            ClientMessage::Event {
                seq: 1,
                event: ExamEvent::Incident(ClientIncident {
                    code: IncidentCode::NavigationDenied,
                    occurred_at: now(),
                    detail: Some("test evidence".into()),
                }),
            },
        )
        .await
        .unwrap();
    let (status, incidents) = fixture
        .request(
            Method::GET,
            &format!("{base}/incidents?oj_user_id=12"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(incidents.as_array().unwrap().len(), 1);
    assert_eq!(incidents[0]["source"], "client");
    assert_eq!(incidents[0]["oj_user_id"], "12");
    assert_eq!(
        fixture
            .request(
                Method::GET,
                &format!("{base}/incidents?oj_user_id=13"),
                None
            )
            .await,
        (StatusCode::OK, json!([]))
    );
    let student = format!("{base}/students/12");
    let (status, events) = fixture
        .request(Method::GET, &format!("{student}/events"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events[0]["seq"], "1");
    assert_eq!(events[0]["event"]["data"]["detail"], "test evidence");
    assert_eq!(
        fixture
            .request(Method::GET, &format!("{student}/recording"), None)
            .await,
        (StatusCode::NOT_FOUND, json!({"code": "NOT_FOUND"}))
    );
}
