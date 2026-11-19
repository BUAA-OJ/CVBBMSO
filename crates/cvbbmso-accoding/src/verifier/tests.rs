use std::net::Ipv4Addr;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use wiremock::matchers::{any, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{IDENTITY_URL, Verifier};
use crate::{AccodingCookies, AccodingError};

const IDENTITY_PATH: &str = "/api/users/me";
const IDENTITY_BODY_LIMIT: usize = 16 * 1024;

async fn mock_identity(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(IDENTITY_PATH))
        .and(header("Accept", "application/json"))
        .and(header(
            "Cookie",
            "connect.sid=s%3Asynthetic.sig; _zte_sid_=auxiliary",
        ))
        .respond_with(response)
        .expect(1)
        .mount(server)
        .await;
}

async fn verify(server: &MockServer) -> Result<cvbbmso_protocol::AccodingUser, AccodingError> {
    verify_at(&format!("{}{IDENTITY_PATH}", server.uri())).await
}

async fn verify_at(url: &str) -> Result<cvbbmso_protocol::AccodingUser, AccodingError> {
    let cookies = AccodingCookies::from_pairs([
        ("connect.sid".into(), "s%3Asynthetic.sig".into()),
        ("_zte_sid_".into(), "auxiliary".into()),
    ])
    .unwrap();
    Verifier::new().unwrap().verify_at(url, &cookies).await
}

#[test]
fn production_endpoint_is_the_student_https_origin() {
    let url = reqwest::Url::parse(IDENTITY_URL).unwrap();
    assert_eq!(
        url.origin().ascii_serialization(),
        "https://accoding.buaa.edu.cn"
    );
    assert_eq!(url.path(), IDENTITY_PATH);
}

#[tokio::test]
async fn returns_the_protocol_user_and_sends_all_cookie_pairs() {
    let server = MockServer::start().await;
    mock_identity(
        &server,
        ResponseTemplate::new(200).set_body_raw(
            r#"{"id":12345,"username":"student@example.test","nickname":"学生"}"#,
            "application/json",
        ),
    )
    .await;

    let user = verify(&server).await.unwrap();
    assert_eq!(user.id, 12345);
    assert_eq!(user.username, "student@example.test");
    assert_eq!(user.nickname, "学生");
}

fn padded_identity(size: usize) -> Vec<u8> {
    let mut body =
        br#"{"id":12345,"username":"student@example.test","nickname":"student"}"#.to_vec();
    body.resize(size, b' ');
    body
}

#[tokio::test]
async fn accepts_an_identity_body_at_the_limit() {
    let server = MockServer::start().await;
    mock_identity(
        &server,
        ResponseTemplate::new(200)
            .set_body_raw(padded_identity(IDENTITY_BODY_LIMIT), "application/json"),
    )
    .await;

    assert_eq!(verify(&server).await.unwrap().id, 12345);
}

#[tokio::test]
async fn rejects_a_chunked_identity_body_over_the_limit_without_content_length() {
    // Use a loopback socket because Wiremock buffers response bodies into one frame.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let url = format!("http://{}{IDENTITY_PATH}", listener.local_addr().unwrap());
    let send_response = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        loop {
            assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            line.clear();
        }
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        let body = padded_identity(IDENTITY_BODY_LIMIT + 1);
        for chunk in body.chunks(IDENTITY_BODY_LIMIT / 2 + 1) {
            response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            response.extend_from_slice(chunk);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        stream.get_mut().write_all(&response).await.unwrap();
    });
    assert!(matches!(
        verify_at(&url).await,
        Err(AccodingError::InvalidResponse(None))
    ));
    send_response.await.unwrap();
}

#[tokio::test]
async fn distinguishes_anonymous_sessions() {
    for response in [
        ResponseTemplate::new(200).set_body_raw("", "application/json"),
        ResponseTemplate::new(200).set_body_raw(" \n\t", "application/json"),
        ResponseTemplate::new(401),
    ] {
        let server = MockServer::start().await;
        mock_identity(&server, response).await;
        assert!(matches!(
            verify(&server).await,
            Err(AccodingError::Unauthenticated)
        ));
    }
}

#[tokio::test]
async fn classifies_login_redirect_without_requesting_the_form() {
    let server = MockServer::start().await;
    mock_identity(
        &server,
        ResponseTemplate::new(302).insert_header("Location", "/user/login?returnUrl=%2F"),
    )
    .await;
    Mock::given(path("/user/login"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    assert!(matches!(
        verify(&server).await,
        Err(AccodingError::Unauthenticated)
    ));
}

#[tokio::test]
async fn rejects_not_modified_even_with_a_login_location() {
    let server = MockServer::start().await;
    mock_identity(
        &server,
        ResponseTemplate::new(304).insert_header("Location", "/user/login"),
    )
    .await;

    assert!(matches!(
        verify(&server).await,
        Err(AccodingError::InvalidResponse(None))
    ));
}

#[tokio::test]
async fn never_contacts_a_redirect_target_on_another_origin() {
    let server = MockServer::start().await;
    let redirect_target = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&redirect_target)
        .await;
    mock_identity(
        &server,
        ResponseTemplate::new(302)
            .insert_header("Location", format!("{}/user/login", redirect_target.uri())),
    )
    .await;

    assert!(matches!(
        verify(&server).await,
        Err(AccodingError::InvalidResponse(None))
    ));
}

#[tokio::test]
async fn rejects_unexpected_success_responses_and_invalid_users() {
    let valid_fields = r#""username":"student@example.test","nickname":"student""#;
    let responses = [
        ResponseTemplate::new(204),
        ResponseTemplate::new(200).set_body_string("<html>Maintenance</html>"),
        ResponseTemplate::new(200).set_body_raw(
            r#"[7,"student@example.test","student"]"#,
            "application/json",
        ),
        ResponseTemplate::new(200).set_body_raw("{broken", "application/json"),
        ResponseTemplate::new(200).set_body_raw("{}", "application/json"),
        ResponseTemplate::new(200)
            .set_body_raw(format!(r#"{{"id":0,{valid_fields}}}"#), "application/json"),
        ResponseTemplate::new(200).set_body_raw(
            format!(r#"{{"id":"7",{valid_fields}}}"#),
            "application/json",
        ),
    ];

    for response in responses {
        let server = MockServer::start().await;
        mock_identity(&server, response).await;
        assert!(matches!(
            verify(&server).await,
            Err(AccodingError::InvalidResponse(_))
        ));
    }
}

#[tokio::test]
async fn preserves_the_upstream_http_error() {
    let server = MockServer::start().await;
    mock_identity(&server, ResponseTemplate::new(503)).await;
    let Err(AccodingError::Upstream(source)) = verify(&server).await else {
        panic!("expected an upstream error");
    };
    assert_eq!(
        source.status(),
        Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)
    );
}
