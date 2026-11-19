//! Exercises HTTPS and WSS against a loopback monitor with synthetic sessions.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{
        Path,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
    serve::Listener,
};
use cvbbmso_accoding::AccodingCookies;
use cvbbmso_protocol::{
    AccodingUser, ClientHello, ClientMessage, ClientState, ExamListResponse, ExamPhase,
    ExamSummary, ExecutionState, NetworkState, Platform, RecordingPolicy, RecordingState,
    ServerMessage, error_code,
};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
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

struct TlsFixture {
    acceptor: TlsAcceptor,
    tcp: TcpListener,
}

impl Listener for TlsFixture {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = self.tcp.accept().await.unwrap();
            if let Ok(Ok(stream)) = timeout(LIMIT, self.acceptor.accept(stream)).await {
                return (stream, addr);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

async fn fixture() -> (SocketAddr, CertificateDer<'static>, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(["127.0.0.1".to_owned()]).unwrap();
    let cert = cert.der().clone();
    let tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.clone()],
            PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
        )
        .unwrap();
    let listener = TlsFixture {
        acceptor: TlsAcceptor::from(Arc::new(tls)),
        tcp: listener,
    };
    let app = Router::new()
        .route(
            "/api/health",
            get(|| async { Json(json!({ "status": "ok" })) }),
        )
        .route("/api/exams", get(exam_list))
        .route("/api/exams/{exam_id}/ws", get(upgrade));
    let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    (addr, cert, server)
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
    assert!(list.items.is_empty());
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
