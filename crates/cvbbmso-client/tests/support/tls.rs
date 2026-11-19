//! Loopback HTTPS endpoint for test fixtures.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{Router, serve::Listener};
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

const LIMIT: Duration = Duration::from_secs(5);

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

pub(crate) async fn serve(app: Router) -> (SocketAddr, CertificateDer<'static>, JoinHandle<()>) {
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
    let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    (addr, cert, server)
}
