use cvbbmso_accoding::AccodingCookies;
use serde_json::json;
use url::Url;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

use super::{MAX_EXAM_LIST_BYTES, Monitor};
use crate::ExamError;

const EXAM: Uuid = Uuid::from_u128(0x10);
const COOKIE: &str = "connect.sid=session; _aux=1";

fn cookies() -> AccodingCookies {
    AccodingCookies::from_pairs([
        ("connect.sid".to_owned(), "session".to_owned()),
        ("_aux".to_owned(), "1".to_owned()),
    ])
    .unwrap()
}

fn list(exam: Uuid) -> serde_json::Value {
    json!({
        "user": {"id": 42, "username": "student", "nickname": "Student"},
        "items": [{
            "id": exam,
            "title": "Final",
            "start_at": "2026-12-06T01:00:00Z",
            "end_at": "2026-12-06T04:00:00Z",
            "phase": "scheduled"
        }]
    })
}

async fn monitor_answering(response: ResponseTemplate) -> (MockServer, Monitor) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/exams"))
        .and(header("cookie", COOKIE))
        .respond_with(response)
        .mount(&server)
        .await;
    let monitor = Monitor::new(Url::parse(&server.uri()).unwrap()).unwrap();
    (server, monitor)
}

#[tokio::test]
async fn enrollment_returns_the_user_and_the_exam() {
    let (_server, monitor) =
        monitor_answering(ResponseTemplate::new(200).set_body_json(list(EXAM))).await;

    let enrollment = monitor.enrollment(&cookies(), EXAM).await.unwrap();

    assert_eq!(enrollment.oj_user_id, 42);
    assert_eq!(enrollment.exam.id, EXAM);
}

#[tokio::test]
async fn an_exam_missing_from_the_list_is_not_enrolled() {
    let (_server, monitor) =
        monitor_answering(ResponseTemplate::new(200).set_body_json(list(Uuid::from_u128(9)))).await;

    let error = monitor.enrollment(&cookies(), EXAM).await.unwrap_err();

    assert!(matches!(error, ExamError::NotEnrolled), "{error:?}");
}

#[tokio::test]
async fn statuses_map_to_runtime_categories() {
    for (status, expected) in [(401, "auth"), (403, "not_enrolled"), (503, "http")] {
        let (_server, monitor) = monitor_answering(ResponseTemplate::new(status)).await;

        let error = monitor.enrollment(&cookies(), EXAM).await.unwrap_err();

        let actual = match error {
            ExamError::Auth => "auth",
            ExamError::NotEnrolled => "not_enrolled",
            ExamError::Http(_) => "http",
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(actual, expected, "status {status}");
    }
}

#[tokio::test]
async fn malformed_or_oversized_bodies_are_rejected() {
    let oversized = format!("{{\"padding\":\"{}\"}}", "x".repeat(MAX_EXAM_LIST_BYTES));
    for body in ["[]".to_owned(), "{\"user\":{}}".to_owned(), oversized] {
        let (_server, monitor) =
            monitor_answering(ResponseTemplate::new(200).set_body_string(body)).await;

        let error = monitor.enrollment(&cookies(), EXAM).await.unwrap_err();

        assert!(matches!(error, ExamError::Http(None)), "{error:?}");
    }
}

#[test]
fn the_socket_request_targets_the_exam_and_carries_the_session() {
    let monitor = Monitor::new(Url::parse("https://monitor.example.org:8443/").unwrap()).unwrap();

    let request = monitor.socket_request(&cookies(), EXAM).unwrap();

    assert_eq!(
        request.uri().to_string(),
        format!("wss://monitor.example.org:8443/api/exams/{EXAM}/ws")
    );
    assert_eq!(request.headers()["cookie"], COOKIE);
}

#[test]
fn only_http_origins_are_accepted() {
    for base in ["ftp://monitor.example.org/", "data:text/plain,monitor"] {
        assert!(Monitor::new(Url::parse(base).unwrap()).is_err(), "{base}");
    }
}
