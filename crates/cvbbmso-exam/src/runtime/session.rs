//! One WebSocket connection to the monitor.
//!
//! A session task owns the socket so that heartbeats keep flowing while the
//! runtime is busy, for example while the recorder merges a long recording.
//! The runtime talks to it through channels; every report carries the
//! session's generation so the runtime can ignore a connection it replaced.

use std::time::Duration;

use cvbbmso_protocol::{ClientMessage, ServerMessage};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{Instant, MissedTickBehavior, interval_at, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, handshake::client::Request},
};

use crate::{ExamError, monitor::handshake_error};

/// Capacity of the queue of messages waiting for the socket.
const OUTBOUND_CAPACITY: usize = 64;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Report from a session task to the runtime.
#[derive(Debug)]
pub(super) enum Inbound {
    /// The handshake succeeded; messages sent here reach the socket in order.
    Opened(u64, mpsc::Sender<ClientMessage>),
    /// A message arrived from the server.
    Message(u64, ServerMessage),
    /// The connection ended; `None` when the runtime closed it.
    Closed(u64, Option<ExamError>),
}

/// Heartbeat cadence and the silence after which a connection counts as dead.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Liveness {
    pub(crate) heartbeat: Duration,
    pub(crate) silence: Duration,
}

/// Connects in the background and reports to `inbound`.
///
/// The session ends when the server closes, the socket fails, nothing arrives
/// for `liveness.silence`, or the runtime drops the outbound sender.
pub(super) fn spawn(
    request: Request,
    generation: u64,
    inbound: mpsc::Sender<Inbound>,
    liveness: Liveness,
) {
    tokio::spawn(async move {
        let socket = match timeout(liveness.silence, connect_async(request)).await {
            Ok(Ok((socket, _))) => socket,
            Ok(Err(error)) => {
                let _ = inbound
                    .send(Inbound::Closed(generation, Some(handshake_error(error))))
                    .await;
                return;
            }
            Err(_) => {
                let _ = inbound
                    .send(Inbound::Closed(
                        generation,
                        Some(ExamError::WebSocket(None)),
                    ))
                    .await;
                return;
            }
        };
        let (outbound, receiver) = mpsc::channel(OUTBOUND_CAPACITY);
        if inbound
            .send(Inbound::Opened(generation, outbound))
            .await
            .is_err()
        {
            return;
        }
        let reason = drive(socket, receiver, &inbound, generation, liveness).await;
        let _ = inbound.send(Inbound::Closed(generation, reason)).await;
    });
}

async fn drive(
    mut socket: Socket,
    mut outbound: mpsc::Receiver<ClientMessage>,
    inbound: &mpsc::Sender<Inbound>,
    generation: u64,
    liveness: Liveness,
) -> Option<ExamError> {
    // The first beat waits a full period so `hello` stays the first message.
    let mut beat = interval_at(Instant::now() + liveness.heartbeat, liveness.heartbeat);
    beat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_seen = Instant::now();
    loop {
        tokio::select! {
            frame = socket.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    last_seen = Instant::now();
                    let Ok(message) = serde_json::from_str::<ServerMessage>(&text) else {
                        tracing::warn!("monitor sent a message outside the protocol");
                        let _ = socket.close(None).await;
                        return Some(ExamError::WebSocket(None));
                    };
                    if inbound.send(Inbound::Message(generation, message)).await.is_err() {
                        return None;
                    }
                }
                Some(Ok(Message::Close(_))) | None => return Some(ExamError::WebSocket(None)),
                Some(Ok(_)) => last_seen = Instant::now(),
                Some(Err(error)) => return Some(ExamError::WebSocket(Some(error))),
            },
            message = outbound.recv() => {
                let Some(message) = message else {
                    let _ = socket.close(None).await;
                    return None;
                };
                if let Err(error) = send(&mut socket, &message).await {
                    return Some(error);
                }
            }
            _ = beat.tick() => {
                if last_seen.elapsed() >= liveness.silence {
                    return Some(ExamError::WebSocket(None));
                }
                if let Err(error) = send(&mut socket, &ClientMessage::Heartbeat).await {
                    return Some(error);
                }
                // A ping makes the server answer even when it has nothing to say,
                // so a half-open connection is noticed within `silence`.
                if let Err(error) = socket.send(Message::Ping(Vec::new().into())).await {
                    return Some(ExamError::WebSocket(Some(error)));
                }
            }
        }
    }
}

async fn send(socket: &mut Socket, message: &ClientMessage) -> Result<(), ExamError> {
    // Protocol messages always serialize; a failure would be a schema bug.
    let text = serde_json::to_string(message).map_err(|_| ExamError::WebSocket(None))?;
    socket
        .send(Message::text(text))
        .await
        .map_err(|error| ExamError::WebSocket(Some(error)))
}
