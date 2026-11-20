use std::time::Duration;

use cvbbmso_protocol::ClientMessage;
use tokio::{io::duplex, sync::mpsc, time::timeout};
use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};

use super::{Liveness, drive};
use crate::ExamError;

#[tokio::test(start_paused = true)]
async fn a_blocked_socket_write_expires_and_closes_the_outbound_queue() {
    // The peer stays open but cannot fit a frame and never reads.
    let (client, _peer) = duplex(1);
    let mut socket = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let (outbound, receiver) = mpsc::channel(1);
    let (inbound, _reports) = mpsc::channel(1);
    let liveness = Liveness {
        heartbeat: Duration::from_secs(5),
        silence: Duration::from_secs(20),
    };
    outbound.try_send(ClientMessage::Heartbeat).unwrap();

    let error = timeout(
        liveness.silence + Duration::from_secs(1),
        drive(&mut socket, receiver, &inbound, 1, liveness),
    )
    .await
    .expect("the blocked write ends at the liveness deadline")
    .unwrap_err();

    assert!(matches!(error, ExamError::WebSocket(None)));
    assert!(outbound.is_closed());
}
