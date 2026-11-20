use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use cvbbmso_domain::DisconnectReason;
use cvbbmso_protocol::{ClientHello, ClientMessage, ServerMessage};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::{
    sync::{mpsc, oneshot},
    time::{self, Instant},
};
use tokio_tungstenite::tungstenite::{Error as WebSocketError, error::ProtocolError};
use tracing::Instrument;
use uuid::Uuid;

use crate::hub::{Connection, Hub, OUTBOUND_CAPACITY, Peer, SocketError, StudentKey};

pub(crate) fn message_limit(exam_duration: ::time::Duration) -> usize {
    // Budget at least one segment per second, including fractional timestamps,
    // and leave room for extra segments during capture recovery and finalizing.
    let seconds = usize::try_from(exam_duration.whole_seconds()).unwrap_or(usize::MAX);
    (2 * 1024 * 1024).max(seconds.saturating_add(60).saturating_mul(256))
}

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
            tracing::warn!(
                code = cvbbmso_protocol::error_code::PROTOCOL_ERROR,
                "WebSocket handshake rejected"
            );
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
            read_frames(&mut reader, &hub, connection, deadline).await;
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
        Ok(Some((ClientMessage::Hello(hello), _))) => Ok(Some(hello)),
        Ok(None) => Ok(None),
        Err(ReadError::Malformed) => Err(SocketError::PROTOCOL_ERROR),
        Ok(Some(_)) | Err(ReadError::Timeout) => Err(SocketError::HELLO_REQUIRED),
    }
}

async fn read_frames(
    reader: &mut (impl Stream<Item = Result<Message, axum::Error>> + Unpin),
    hub: &Hub,
    connection: Connection,
    mut deadline: Instant,
) {
    let (reason, error) = loop {
        match next_message(reader, deadline).await {
            Ok(Some((ClientMessage::Hello(_), _))) | Err(ReadError::Malformed) => {
                break (DisconnectReason::Closed, Some(SocketError::PROTOCOL_ERROR));
            }
            Ok(Some((message, received_at))) => {
                let Some(renewed) = hub.receive(connection, message, received_at).await else {
                    return;
                };
                deadline = renewed;
            }
            Ok(None) => break (DisconnectReason::Closed, None),
            Err(ReadError::Timeout) => break (DisconnectReason::HeartbeatTimeout, None),
        }
    };
    hub.end(connection, reason, error).await;
}

enum ReadError {
    Timeout,
    Malformed,
}

async fn next_message(
    reader: &mut (impl Stream<Item = Result<Message, axum::Error>> + Unpin),
    deadline: Instant,
) -> Result<Option<(ClientMessage, Instant)>, ReadError> {
    loop {
        let frame = time::timeout_at(deadline, reader.next())
            .await
            .map_err(|_| ReadError::Timeout)?;
        let received_at = Instant::now();
        if received_at >= deadline {
            return Err(ReadError::Timeout);
        }
        match frame {
            Some(Ok(Message::Text(text))) => {
                let message = serde_json::from_str(&text).map_err(|_| ReadError::Malformed)?;
                return Ok(Some((message, received_at)));
            }
            Some(Ok(Message::Binary(_))) => return Err(ReadError::Malformed),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Err(error)) if malformed_frame(&error) => return Err(ReadError::Malformed),
            Some(Ok(Message::Close(_)) | Err(_)) | None => return Ok(None),
        }
    }
}

fn malformed_frame(error: &axum::Error) -> bool {
    let error = std::error::Error::source(error).and_then(|source| source.downcast_ref());
    matches!(
        error,
        Some(WebSocketError::Utf8(_) | WebSocketError::Protocol(_) | WebSocketError::Capacity(_))
    ) && !matches!(
        error,
        Some(WebSocketError::Protocol(
            ProtocolError::ResetWithoutClosingHandshake
        ))
    )
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
