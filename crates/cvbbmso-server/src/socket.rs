use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use cvbbmso_domain::DisconnectReason;
use cvbbmso_protocol::{ClientHello, ClientMessage, ServerMessage};
use futures_util::{Sink, SinkExt, Stream, StreamExt, stream::FuturesUnordered};
use tokio::{
    sync::{mpsc, oneshot},
    time::{self, Instant},
};
use tokio_tungstenite::tungstenite::{Error as WebSocketError, error::ProtocolError};
use tracing::Instrument;
use uuid::Uuid;

use crate::hub::{Connection, Hub, OUTBOUND_CAPACITY, Peer, SocketError, StudentKey};

pub(crate) async fn run(
    socket: WebSocket,
    hub: Hub,
    student: StudentKey,
    request_id: Uuid,
    timeout: Duration,
) {
    let connection = Connection {
        student,
        id: Uuid::new_v4(),
        request_id,
    };
    session(socket, hub, connection, timeout)
        .instrument(connection.span())
        .await;
}

async fn session(mut socket: WebSocket, hub: Hub, connection: Connection, timeout: Duration) {
    let hello = match first_message(&mut socket, timeout).await {
        Ok(Some(hello)) => hello,
        Ok(None) => return,
        Err(error) => {
            error.log_rejection("WebSocket handshake rejected");
            if send_message(&mut socket, error.frame(), timeout)
                .await
                .is_ok()
            {
                let _ = time::timeout(timeout, socket.send(Message::Close(None))).await;
            }
            return;
        }
    };

    let (frames, outgoing) = mpsc::channel(OUTBOUND_CAPACITY);
    let (close, closing) = oneshot::channel();
    let (admitted, admission) = oneshot::channel();
    if !hub
        .connect(
            connection,
            hello,
            Peer {
                frames,
                close,
                admitted,
            },
        )
        .await
    {
        return;
    }

    let (mut writer, mut reader) = socket.split();
    let write = write_frames(&mut writer, outgoing, closing, timeout);
    let read = async {
        if let Ok(deadline) = admission.await {
            read_frames(&mut reader, &hub, connection, deadline, timeout).await;
        }
    };
    tokio::pin!(write);
    tokio::select! {
        () = &mut write => hub.end(connection, DisconnectReason::Closed, None).await,
        () = read => write.await,
    }
}

async fn first_message(
    reader: &mut (impl Stream<Item = Result<Message, axum::Error>> + Unpin),
    timeout: Duration,
) -> Result<Option<ClientHello>, SocketError> {
    match next_message(reader, Instant::now() + timeout).await {
        Ok(Some((ClientMessage::Hello(hello), _, _))) => Ok(Some(hello)),
        Ok(None) => Ok(None),
        Err(ReadError::Malformed(reason)) => Err(SocketError::protocol(reason)),
        Ok(Some(_)) => Err(SocketError::hello_required("first message was not hello")),
        Err(ReadError::Timeout) => Err(SocketError::hello_required("timed out waiting for hello")),
    }
}

pub(super) async fn read_frames(
    reader: &mut (impl Stream<Item = Result<Message, axum::Error>> + Unpin),
    hub: &Hub,
    connection: Connection,
    mut deadline: Instant,
    timeout: Duration,
) {
    let max_pending = hub.max_per_conn_pending();
    let mut in_flight: FuturesUnordered<oneshot::Receiver<Option<Instant>>> =
        FuturesUnordered::new();

    let (reason, error) = loop {
        // Apply TCP backpressure when this connection has reached its pending reliable message cap.
        // Do not expire the heartbeat timer while reading is paused for server capacity.
        if in_flight.len() >= max_pending {
            match in_flight.next().await {
                Some(Ok(Some(renewed))) => {
                    deadline = deadline.max(renewed).max(Instant::now() + timeout);
                    continue;
                }
                Some(_) => return,
                None => continue,
            }
        }

        let incoming = tokio::select! {
            biased;
            completed = in_flight.next(), if !in_flight.is_empty() => {
                match completed {
                    Some(Ok(Some(renewed))) => {
                        deadline = deadline.max(renewed);
                        continue;
                    }
                    Some(_) => return,
                    None => continue,
                }
            }
            incoming = next_message(reader, deadline) => incoming,
        };

        let (message, received_at, bytes) = match incoming {
            Ok(Some((ClientMessage::Hello(_), _, _))) => {
                break (
                    DisconnectReason::Closed,
                    Some(SocketError::protocol("repeated hello")),
                );
            }
            Err(ReadError::Malformed(reason)) => {
                break (
                    DisconnectReason::Closed,
                    Some(SocketError::protocol(reason)),
                );
            }
            Ok(Some(message)) => message,
            Ok(None) => break (DisconnectReason::Closed, None),
            Err(ReadError::Timeout) => break (DisconnectReason::HeartbeatTimeout, None),
        };

        if matches!(message, ClientMessage::Heartbeat) {
            let Some(renewed) = hub.heartbeat(connection, received_at).await else {
                return;
            };
            deadline = deadline.max(renewed);
            continue;
        }

        let permit = if let Some(permit) = hub.budget().try_acquire(bytes) {
            permit
        } else {
            let acquire = hub.budget().acquire(bytes);
            tokio::pin!(acquire);
            loop {
                tokio::select! {
                    biased;
                    completed = in_flight.next(), if !in_flight.is_empty() => {
                        match completed {
                            Some(Ok(Some(renewed))) => deadline = deadline.max(renewed),
                            Some(_) => return,
                            None => {}
                        }
                    }
                    permit = &mut acquire => {
                        deadline = deadline.max(Instant::now() + timeout);
                        break permit;
                    }
                }
            }
        };

        let Some(completion) = hub
            .enqueue_reliable(connection, message, received_at, permit)
            .await
        else {
            return;
        };
        in_flight.push(completion);
    };
    hub.end(connection, reason, error).await;
}

enum ReadError {
    Timeout,
    Malformed(&'static str),
}

struct ReceivedMessage {
    received_at: Instant,
    bytes: usize,
    message: Result<Option<ClientMessage>, &'static str>,
}

impl ReceivedMessage {
    fn before(
        self,
        deadline: Instant,
    ) -> Result<Option<(ClientMessage, Instant, usize)>, ReadError> {
        if self.received_at >= deadline {
            return Err(ReadError::Timeout);
        }
        self.message
            .map(|message| message.map(|message| (message, self.received_at, self.bytes)))
            .map_err(ReadError::Malformed)
    }
}

async fn next_message(
    reader: &mut (impl Stream<Item = Result<Message, axum::Error>> + Unpin),
    deadline: Instant,
) -> Result<Option<(ClientMessage, Instant, usize)>, ReadError> {
    time::timeout_at(deadline, read_message(reader))
        .await
        .map_err(|_| ReadError::Timeout)?
        .before(deadline)
}

async fn read_message(
    reader: &mut (impl Stream<Item = Result<Message, axum::Error>> + Unpin),
) -> ReceivedMessage {
    loop {
        let frame = reader.next().await;
        let received_at = Instant::now();
        let mut bytes = 0usize;
        let message = match frame {
            Some(Ok(Message::Text(text))) => {
                bytes = text.len();
                serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|_| "invalid JSON message")
            }
            Some(Ok(Message::Binary(_))) => Err("binary frame received"),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                // A stream of ready control frames must still yield to the read timeout.
                tokio::task::consume_budget().await;
                continue;
            }
            Some(Err(error)) => frame_error(&error).map_or(Ok(None), Err),
            Some(Ok(Message::Close(_))) | None => Ok(None),
        };
        return ReceivedMessage {
            received_at,
            bytes,
            message,
        };
    }
}

fn frame_error(error: &axum::Error) -> Option<&'static str> {
    let error = std::error::Error::source(error).and_then(|source| source.downcast_ref());
    match error {
        Some(WebSocketError::Utf8(_)) => Some("invalid frame text encoding"),
        Some(WebSocketError::Capacity(_)) => Some("frame or message exceeded size limit"),
        Some(WebSocketError::Protocol(ProtocolError::ResetWithoutClosingHandshake)) => None,
        Some(WebSocketError::Protocol(_)) => Some("invalid WebSocket frame"),
        _ => None,
    }
}

async fn write_frames(
    writer: &mut (impl Sink<Message, Error = axum::Error> + Unpin),
    mut frames: mpsc::Receiver<ServerMessage>,
    mut close: oneshot::Receiver<Option<SocketError>>,
    timeout: Duration,
) {
    let terminal = loop {
        tokio::select! {
            biased;
            terminal = &mut close => break terminal.ok().flatten(),
            frame = frames.recv() => {
                let Some(frame) = frame else {
                    break close.await.ok().flatten();
                };
                if send_message(writer, frame, timeout).await.is_err() {
                    tracing::warn!("WebSocket write failed");
                    return;
                }
            }
        }
    };
    if let Some(error) = terminal {
        while let Ok(frame) = frames.try_recv() {
            if send_message(writer, frame, timeout).await.is_err() {
                return;
            }
        }
        if send_message(writer, error.frame(), timeout).await.is_err() {
            return;
        }
    }
    let _ = time::timeout(timeout, writer.send(Message::Close(None))).await;
}

async fn send_message(
    writer: &mut (impl Sink<Message, Error = axum::Error> + Unpin),
    message: ServerMessage,
    timeout: Duration,
) -> Result<(), ()> {
    let text = serde_json::to_string(&message).map_err(|_| ())?;
    time::timeout(timeout, writer.send(Message::Text(text.into())))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn ping_and_pong_do_not_extend_the_hello_deadline() {
        let frames = futures_util::stream::iter([
            Ok(Message::Ping(axum::body::Bytes::new())),
            Ok(Message::Pong(axum::body::Bytes::new())),
        ]);
        let mut reader = frames.chain(futures_util::stream::pending());
        let accepted_at = Instant::now();
        assert!(
            first_message(&mut reader, Duration::from_secs(25))
                .await
                .is_err()
        );
        assert_eq!(accepted_at.elapsed(), Duration::from_secs(25));
    }
}
