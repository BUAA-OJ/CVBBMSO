use cvbbmso_accoding::AccodingCookies;
use cvbbmso_protocol::ExamPhase;
use serde_json::json;
use url::Url;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

use super::Monitor;
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
    let monitor = Monitor::for_test(Url::parse(&server.uri()).unwrap()).unwrap();
    (server, monitor)
}

#[tokio::test]
async fn enrollment_returns_the_user_id_and_exam_phase() {
    let (_server, monitor) =
        monitor_answering(ResponseTemplate::new(200).set_body_json(list(EXAM))).await;

    assert_eq!(
        monitor.enrollment(&cookies(), EXAM).await.unwrap(),
        (42, ExamPhase::Scheduled)
    );
}

#[tokio::test]
async fn an_exam_missing_from_the_list_is_not_enrolled() {
    let (_server, monitor) =
        monitor_answering(ResponseTemplate::new(200).set_body_json(list(Uuid::from_u128(9)))).await;

    let error = monitor.enrollment(&cookies(), EXAM).await.unwrap_err();

    assert!(matches!(error, ExamError::NotEnrolled), "{error:?}");
}

#[tokio::test]
async fn responses_map_to_runtime_categories() {
    for (status, expected) in [
        (201, "http"),
        (401, "auth"),
        (403, "not_enrolled"),
        (404, "http"),
        (503, "http"),
    ] {
        let (_server, monitor) = monitor_answering(ResponseTemplate::new(status)).await;

        let error = monitor.enrollment(&cookies(), EXAM).await.unwrap_err();

        let actual = match error {
            ExamError::Auth => "auth",
            ExamError::NotEnrolled => "not_enrolled",
            ExamError::Http(None) => "http",
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(actual, expected, "status {status}");
    }

    let (_server, monitor) =
        monitor_answering(ResponseTemplate::new(200).set_body_json(json!({"user": {}}))).await;
    assert!(matches!(
        monitor.enrollment(&cookies(), EXAM).await,
        Err(ExamError::Http(None))
    ));
}

#[test]
fn the_socket_request_targets_the_exam_and_carries_the_session() {
    let monitor = Monitor::new(
        Url::parse("https://monitor.example.org:8443/").unwrap(),
        None,
    )
    .unwrap();

    let request = monitor.socket_request(&cookies(), EXAM).unwrap();

    assert_eq!(
        request.uri().to_string(),
        format!("wss://monitor.example.org:8443/api/exams/{EXAM}/ws")
    );
    assert_eq!(request.headers()["cookie"], COOKIE);
}

#[test]
fn only_https_origins_are_accepted() {
    for base in [
        "http://monitor.example.org/",
        "ftp://monitor.example.org/",
        "data:text/plain,monitor",
        "https://monitor.example.org/path",
        "https://monitor.example.org/?query",
        "https://monitor.example.org/#fragment",
        "https://user:password@monitor.example.org/",
    ] {
        assert!(
            Monitor::new(Url::parse(base).unwrap(), None).is_err(),
            "{base}"
        );
    }
}
