//! One WebSocket connection to the monitor.
//!
//! A session task owns the socket so that heartbeats keep flowing while the
//! runtime is busy, for example while the recorder merges a long recording.
//! The runtime talks to it through channels; every report carries the
//! session's generation so the runtime can ignore a connection it replaced.

use std::{
    hash::{BuildHasher, Hasher, RandomState},
    time::Duration,
};

use cvbbmso_protocol::{ClientMessage, ServerMessage};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
    time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout, timeout_at},
};
use tokio_tungstenite::{
    WebSocketStream, connect_async,
    tungstenite::{Message, handshake::client::Request},
};

use crate::{ExamError, monitor::handshake_error};

/// Capacity of the queue of messages waiting for the socket.
const OUTBOUND_CAPACITY: usize = 64;

/// Report from a session task to the runtime.
#[derive(Debug)]
pub(super) enum Inbound {
    /// The handshake succeeded; messages sent here reach the socket in order.
    Opened(u64, mpsc::Sender<ClientMessage>),
    /// A message arrived from the server.
    Message {
        generation: u64,
        message: ServerMessage,
        received_at: Instant,
    },
    /// The connection ended; `None` when its channels were closed normally.
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
/// for `liveness.silence`, or the runtime drops the cancellation sender.
pub(super) fn spawn(
    request: Request,
    generation: u64,
    inbound: mpsc::Sender<Inbound>,
    liveness: Liveness,
) -> oneshot::Sender<()> {
    let (cancel, mut cancelled) = oneshot::channel();
    tokio::spawn(async move {
        let handshake = tokio::select! {
            biased;
            _ = &mut cancelled => return,
            result = timeout(liveness.silence, connect_async(request)) => result,
        };
        let reason = match handshake {
            Ok(Ok((mut socket, _))) => {
                let (outbound, receiver) = mpsc::channel(OUTBOUND_CAPACITY);
                tokio::select! {
                    biased;
                    _ = &mut cancelled => {
                        // Send a close frame if the socket is writable.
                        let _ = timeout(Duration::ZERO, socket.close(None)).await;
                        return;
                    }
                    result = async {
                        if inbound.send(Inbound::Opened(generation, outbound)).await.is_err() {
                            return Ok(());
                        }
                        drive(&mut socket, receiver, &inbound, generation, liveness).await
                    } => result,
                }
            }
            Ok(Err(error)) => Err(handshake_error(error)),
            Err(_) => Err(ExamError::WebSocket(None)),
        };
        tokio::select! {
            _ = &mut cancelled => {}
            _ = inbound.send(Inbound::Closed(generation, reason.err())) => {}
        }
    });
    cancel
}

async fn drive<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    mut outbound: mpsc::Receiver<ClientMessage>,
    inbound: &mpsc::Sender<Inbound>,
    generation: u64,
    liveness: Liveness,
) -> Result<(), ExamError> {
    let mut deadline = Instant::now() + liveness.silence;
    // A delayed runtime cannot cause the first heartbeat to overtake hello.
    let Some(hello) = timeout_at(deadline, outbound.recv())
        .await
        .map_err(|_| ExamError::WebSocket(None))?
    else {
        return Ok(());
    };
    send_message(socket, &hello, deadline).await?;

    let mut synced = false;
    let mut beat = interval_at(Instant::now() + liveness.heartbeat, liveness.heartbeat);
    beat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = sleep_until(deadline) => return Err(ExamError::WebSocket(None)),
            frame = socket.next() => {
                let seen = Instant::now();
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(message) = serde_json::from_str::<ServerMessage>(&text) else {
                            tracing::warn!("monitor sent a message outside the protocol");
                            return Err(ExamError::WebSocket(None));
                        };
                        let rejected = !synced && matches!(message, ServerMessage::Error { .. });
                        if let ServerMessage::Sync { start_at, end_at, .. } = &message {
                            if synced || start_at > end_at {
                                return Err(ExamError::WebSocket(None));
                            }
                            synced = true;
                            let phase = initial_heartbeat_phase(liveness.heartbeat, random_u64());
                            beat = interval_at(Instant::now() + phase, liveness.heartbeat);
                            beat.set_missed_tick_behavior(MissedTickBehavior::Delay);
                        } else if !synced && !rejected {
                            return Err(ExamError::WebSocket(None));
                        }
                        if synced {
                            deadline = seen + liveness.silence;
                        }
                        let report = Inbound::Message { generation, message, received_at: seen };
                        if timeout_at(deadline, inbound.send(report))
                            .await.map_err(|_| ExamError::WebSocket(None))?.is_err()
                        {
                            return Ok(());
                        }
                        if rejected {
                            return Ok(());
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return Err(ExamError::WebSocket(None)),
                    Some(Ok(_)) => {
                        if synced {
                            deadline = seen + liveness.silence;
                        }
                    }
                    Some(Err(error)) => return Err(ExamError::WebSocket(Some(error))),
                }
            }
            message = outbound.recv() => {
                let Some(message) = message else {
                    return Ok(());
                };
                send_message(socket, &message, deadline).await?;
            }
            _ = beat.tick(), if synced => {
                send_message(socket, &ClientMessage::Heartbeat, deadline).await?;
                // A ping makes the server answer even when it has no business messages.
                write_frame(socket, Message::Ping(Vec::new().into()), deadline).await?;
            }
        }
    }
}

async fn send_message<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    message: &ClientMessage,
    deadline: Instant,
) -> Result<(), ExamError> {
    // Protocol messages always serialize; a failure would be a schema bug.
    let text = serde_json::to_string(message).map_err(|_| ExamError::WebSocket(None))?;
    write_frame(socket, Message::text(text), deadline).await
}

async fn write_frame<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    frame: Message,
    deadline: Instant,
) -> Result<(), ExamError> {
    timeout_at(deadline, socket.send(frame))
        .await
        .map_err(|_| ExamError::WebSocket(None))?
        .map_err(|error| ExamError::WebSocket(Some(error)))
}

fn random_u64() -> u64 {
    RandomState::new().build_hasher().finish()
}

fn initial_heartbeat_phase(heartbeat: Duration, sample: u64) -> Duration {
    let millis = u64::try_from(heartbeat.as_millis()).unwrap_or(u64::MAX);
    if millis <= 1 {
        return heartbeat;
    }
    let offset = (sample % millis).saturating_add(1);
    Duration::from_millis(offset).min(heartbeat)
}

#[cfg(test)]
mod tests;
