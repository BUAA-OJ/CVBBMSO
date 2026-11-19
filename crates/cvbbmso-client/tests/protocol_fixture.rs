//! Exercises HTTPS and WSS against a loopback monitor with synthetic sessions.

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[path = "../src/monitor.rs"]
mod monitor;
#[path = "support/tls.rs"]
mod tls;

use monitor::{MAX_EXAM_LIST_BYTES, Monitor, MonitorError};

use axum::{
    Json, Router,
    body::Body,
    extract::{
        Path,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use cvbbmso_accoding::AccodingCookies;
use cvbbmso_protocol::{
    AccodingUser, ClientHello, ClientMessage, ClientState, ExamListResponse, ExamPhase,
    ExamSummary, ExecutionState, NetworkState, Platform, RecordingPolicy, RecordingState,
    ServerMessage, error_code,
};
use futures_util::{SinkExt, StreamExt, stream};
use reqwest::Client;
use rustls::{ClientConfig, RootCertStore, pki_types::CertificateDer};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::{task::JoinHandle, time::timeout};
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config, tungstenite::client::IntoClientRequest,
};
use uuid::Uuid;

const EXAM_ID: Uuid = Uuid::from_u128(0x10);
const TIMESTAMP: i64 = 1_797_729_600;
const LIMIT: Duration = Duration::from_secs(5);

fn at(delta: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(TIMESTAMP + delta).unwrap()
}

fn fail(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

fn enrollments(
    headers: &HeaderMap,
) -> Result<ExamListResponse, (StatusCode, &'static str, &'static str)> {
    let items = match headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
    {
        Some("connect.sid=enrolled; _zte_cid_=synthetic") => vec![
            ExamSummary::new(
                EXAM_ID,
                "Synthetic exam".into(),
                at(0),
                at(3600),
                ExamPhase::Scheduled,
            )
            .unwrap(),
        ],
        Some("connect.sid=without-exams; _zte_cid_=synthetic") => vec![],
        Some("connect.sid=upstream-down; _zte_cid_=synthetic") => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                error_code::ACCODING_UNAVAILABLE,
                "Identity service unavailable",
            ));
        }
        _ => {
            return Err((
                StatusCode::UNAUTHORIZED,
                error_code::ACCODING_AUTH_EXPIRED,
                "Accoding login required",
            ));
        }
    };
    Ok(ExamListResponse {
        user: AccodingUser::new(42, "test-student".into(), "Test Student".into()).unwrap(),
        items,
    })
}

async fn exam_list(headers: HeaderMap) -> Response {
    if headers.get(header::COOKIE) == Some(&cookie("malformed").header_value()) {
        return Json(json!({
            "user": {"id": 0, "username": "test", "nickname": "Test"},
            "items": []
        }))
        .into_response();
    }
    if headers.get(header::COOKIE) == Some(&cookie("positional-list").header_value()) {
        return Json(json!([
            {"id": 42, "username": "test", "nickname": "Test"},
            []
        ]))
        .into_response();
    }
    match enrollments(&headers) {
        Ok(list) => Json(list).into_response(),
        Err((status, code, message)) => fail(status, code, message),
    }
}

async fn upgrade(Path(exam_id): Path<Uuid>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    match enrollments(&headers) {
        Ok(list) if list.items.iter().any(|exam| exam.id == exam_id) => ws.on_upgrade(first_frame),
        Ok(_) => fail(
            StatusCode::FORBIDDEN,
            error_code::NOT_ENROLLED,
            "Student is not enrolled",
        ),
        Err((status, code, message)) => fail(status, code, message),
    }
}

async fn first_frame(mut ws: WebSocket) {
    let sync = match ws.recv().await {
        Some(Ok(Message::Text(body)))
            if matches!(serde_json::from_str(&body), Ok(ClientMessage::Hello(_))) =>
        {
            ServerMessage::Sync {
                server_time: at(-5),
                phase: ExamPhase::Scheduled,
                start_at: at(0),
                end_at: at(3600),
                recording_policy: RecordingPolicy::new(30, 15).unwrap(),
                ack_event_through: 0,
            }
        }
        _ => ServerMessage::Error {
            code: error_code::PROTOCOL_ERROR.into(),
            message: "hello must be the first message".into(),
        },
    };
    let _ = ws
        .send(Message::Text(serde_json::to_string(&sync).unwrap().into()))
        .await;
}

async fn fixture() -> (SocketAddr, CertificateDer<'static>, JoinHandle<()>) {
    let app = Router::new()
        .route(
            "/api/health",
            get(|| async { Json(json!({ "status": "ok" })) }),
        )
        .route("/api/exams", get(exam_list))
        .route("/api/exams/{exam_id}/ws", get(upgrade));
    tls::serve(app).await
}

fn cookie(session: &str) -> AccodingCookies {
    AccodingCookies::from_pairs([
        ("connect.sid".into(), session.into()),
        ("_zte_cid_".into(), "synthetic".into()),
    ])
    .unwrap()
}

#[tokio::test]
async fn http_distinguishes_expired_unavailable_and_empty_enrollment() {
    let (addr, cert, server) = fixture().await;
    let client = Client::builder()
        .add_root_certificate(reqwest::Certificate::from_der(cert.as_ref()).unwrap())
        .no_proxy()
        .timeout(LIMIT)
        .build()
        .unwrap();
    let health = client
        .get(format!("https://{addr}/api/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(
        health.json::<Value>().await.unwrap(),
        json!({ "status": "ok" })
    );

    let url = format!("https://{addr}/api/exams");
    for (session, status, code) in [
        (
            "expired",
            StatusCode::UNAUTHORIZED,
            error_code::ACCODING_AUTH_EXPIRED,
        ),
        (
            "upstream-down",
            StatusCode::SERVICE_UNAVAILABLE,
            error_code::ACCODING_UNAVAILABLE,
        ),
    ] {
        let response = client
            .get(&url)
            .header(header::COOKIE, cookie(session).header_value())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            code
        );
    }
    let response = client
        .get(&url)
        .header(header::COOKIE, cookie("without-exams").header_value())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let list = response.json::<ExamListResponse>().await.unwrap();
    assert_eq!(list.user.id, 42);
    assert_eq!(list.items, Vec::<ExamSummary>::new());
    server.abort();
}

#[tokio::test]
async fn websocket_checks_identity_and_answers_hello_with_sync() {
    use tokio_tungstenite::tungstenite::{Error, Message};

    let (addr, cert, server) = fixture().await;
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    let tls = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let request = |session| {
        let mut request = format!("wss://{addr}/api/exams/{EXAM_ID}/ws")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert(header::COOKIE, cookie(session).header_value());
        request
    };
    let connect = |session| {
        connect_async_tls_with_config(
            request(session),
            None,
            false,
            Some(Connector::Rustls(tls.clone())),
        )
    };
    for (session, status) in [
        ("expired", StatusCode::UNAUTHORIZED),
        ("without-exams", StatusCode::FORBIDDEN),
    ] {
        let result = timeout(LIMIT, connect(session)).await.unwrap();
        assert!(matches!(result, Err(Error::Http(response)) if response.status() == status));
    }
    let (mut ws, handshake) = timeout(LIMIT, connect("enrolled")).await.unwrap().unwrap();
    assert_eq!(handshake.status(), StatusCode::SWITCHING_PROTOCOLS);
    let hello = ClientMessage::Hello(ClientHello {
        device_id: Uuid::from_u128(0x20),
        instance_id: Uuid::from_u128(0x30),
        platform: Platform::Windows,
        state: ClientState {
            execution: ExecutionState::Ready,
            recording: RecordingState::Idle,
            network: NetworkState::Clear,
        },
    });
    ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .unwrap();
    let received = timeout(LIMIT, ws.next()).await.unwrap().unwrap().unwrap();
    let Message::Text(body) = received else {
        panic!("Expected a JSON text frame");
    };
    assert_eq!(
        serde_json::from_str::<ServerMessage>(&body).unwrap(),
        ServerMessage::Sync {
            server_time: at(-5),
            phase: ExamPhase::Scheduled,
            start_at: at(0),
            end_at: at(3600),
            recording_policy: RecordingPolicy::new(30, 15).unwrap(),
            ack_event_through: 0,
        }
    );
    server.abort();
}

#[tokio::test]
async fn monitor_validates_the_https_exam_list_and_returns_only_items() {
    let (addr, cert, server) = fixture().await;
    let origin = url::Url::parse(&format!("https://{addr}"))
        .unwrap()
        .origin();
    let root = reqwest::Certificate::from_der(cert.as_ref()).unwrap();
    let monitor = Monitor::with_root_certificate(origin.clone(), root).unwrap();

    let items = monitor.list_exams(&cookie("enrolled")).await.unwrap();
    assert_eq!(
        items,
        [ExamSummary::new(
            EXAM_ID,
            "Synthetic exam".into(),
            at(0),
            at(3600),
            ExamPhase::Scheduled,
        )
        .unwrap()]
    );
    assert_eq!(
        monitor.list_exams(&cookie("without-exams")).await.unwrap(),
        Vec::<ExamSummary>::new()
    );
    assert!(matches!(
        monitor.list_exams(&cookie("expired")).await,
        Err(MonitorError::SignInRequired)
    ));
    assert!(matches!(
        monitor.list_exams(&cookie("upstream-down")).await,
        Err(MonitorError::Unavailable)
    ));
    assert!(matches!(
        monitor.list_exams(&cookie("malformed")).await,
        Err(MonitorError::InvalidResponse(_))
    ));
    assert!(matches!(
        monitor.list_exams(&cookie("positional-list")).await,
        Err(MonitorError::NonObjectResponse)
    ));

    let untrusted = Monitor::new(Some(origin)).unwrap();
    assert!(matches!(
        untrusted.list_exams(&cookie("enrolled")).await,
        Err(MonitorError::Connection(_))
    ));
    assert!(matches!(
        Monitor::new(None),
        Err(MonitorError::Configuration)
    ));
    let plaintext = url::Url::parse("http://127.0.0.1").unwrap().origin();
    assert!(matches!(
        Monitor::new(Some(plaintext)),
        Err(MonitorError::Configuration)
    ));
    server.abort();
}

#[tokio::test]
async fn monitor_accepts_the_size_limit_and_stops_reading_chunked_overflow() {
    let (addr, cert, server) = tls::serve(Router::new().route(
        "/api/exams",
        get(|headers: HeaderMap| async move {
            let mut body =
                br#" {"user":{"id":42,"username":"test","nickname":"Test"},"items":[]}"#.to_vec();
            body.resize(MAX_EXAM_LIST_BYTES, b' ');
            let body = if headers.get(header::COOKIE) == Some(&cookie("at-limit").header_value()) {
                Body::from(body)
            } else {
                // An unknown-length HTTP/1.1 body crosses the cap without ever ending.
                let chunks = [Ok::<_, Infallible>(body), Ok(vec![b' '])];
                Body::from_stream(stream::iter(chunks).chain(stream::pending()))
            };
            ([(header::CONTENT_TYPE, "application/json")], body)
        }),
    ))
    .await;
    let origin = url::Url::parse(&format!("https://{addr}"))
        .unwrap()
        .origin();
    let root = reqwest::Certificate::from_der(cert.as_ref()).unwrap();
    let monitor = Monitor::with_root_certificate(origin, root).unwrap();

    let items = monitor.list_exams(&cookie("at-limit")).await.unwrap();
    assert_eq!(items, Vec::<ExamSummary>::new());
    assert!(matches!(
        timeout(LIMIT, monitor.list_exams(&cookie("oversized")))
            .await
            .expect("the monitor transport must stop before the body ends"),
        Err(MonitorError::ResponseTooLarge)
    ));
    server.abort();
}

#[tokio::test]
async fn monitor_refuses_redirects_even_within_the_configured_origin() {
    let target_hits = Arc::new(AtomicUsize::new(0));
    let hits = target_hits.clone();
    let (addr, cert, server) = tls::serve(
        Router::new()
            .route(
                "/api/exams",
                get(|| async { Redirect::temporary("/unexpected") }),
            )
            .route(
                "/unexpected",
                get(move || async move {
                    hits.fetch_add(1, Ordering::Relaxed);
                    StatusCode::OK
                }),
            ),
    )
    .await;
    let origin = url::Url::parse(&format!("https://{addr}"))
        .unwrap()
        .origin();
    let root = reqwest::Certificate::from_der(cert.as_ref()).unwrap();
    let monitor = Monitor::with_root_certificate(origin, root).unwrap();
    assert!(matches!(
        monitor.list_exams(&cookie("enrolled")).await,
        Err(MonitorError::UnexpectedStatus(
            StatusCode::TEMPORARY_REDIRECT
        ))
    ));
    assert_eq!(target_hits.load(Ordering::Relaxed), 0);
    server.abort();
}
