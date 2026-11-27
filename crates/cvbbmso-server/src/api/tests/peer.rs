//! Direct Hub peers shared by session and acceptance tests.

use tokio::{sync::oneshot, time::Instant};

use super::*;
use crate::hub::{Connection, Peer, SocketError};

// Direct peers let tests order stale notifications and prove that no command
// was emitted without waiting for a real socket or an arbitrary sleep.
pub(super) struct TestPeer {
    pub(super) connection: Connection,
    pub(super) deadline: Instant,
    pub(super) outgoing: mpsc::Receiver<ServerMessage>,
    pub(super) closing: oneshot::Receiver<Option<SocketError>>,
}

impl TestPeer {
    pub(super) async fn connect(
        fixture: &Fixture,
        user: i64,
        execution: ExecutionState,
        recording: RecordingState,
        ack: i64,
    ) -> Self {
        let connection = Connection {
            student: StudentKey {
                exam_id: fixture.exam_id,
                oj_user_id: user,
            },
            id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
        };
        let ClientMessage::Hello(hello) = hello(execution, recording) else {
            unreachable!()
        };
        let (frames, outgoing) = mpsc::channel(crate::hub::OUTBOUND_CAPACITY);
        let (close, closing) = oneshot::channel();
        let (admitted, admission) = oneshot::channel();
        assert!(
            fixture
                .state
                .hub
                .connect(
                    connection,
                    hello,
                    "test@example.test".into(),
                    Peer {
                        frames,
                        close,
                        admitted
                    }
                )
                .await
        );
        let deadline = admission.await.unwrap();
        let mut peer = Self {
            connection,
            deadline,
            outgoing,
            closing,
        };
        assert!(matches!(
            peer.outgoing.recv().await,
            Some(ServerMessage::Sync { ack_event_through, .. }) if ack_event_through == ack
        ));
        peer
    }

    pub(super) async fn receive(&self, fixture: &Fixture, message: ClientMessage) -> bool {
        fixture
            .state
            .hub
            .receive(self.connection, message, Instant::now(), || {})
            .await
            .is_some()
    }

    pub(super) async fn command(&mut self, expected: CommandName, at: i64) -> Uuid {
        let Some(ServerMessage::Command {
            command_id,
            name,
            effective_at,
        }) = self.outgoing.recv().await
        else {
            panic!("expected command")
        };
        assert_eq!(name, expected);
        assert_eq!(effective_at, instant(at));
        command_id
    }

    pub(super) async fn result(&mut self, fixture: &Fixture, id: Uuid, status: CommandStatus) {
        assert!(self.receive(fixture, result(id, status)).await);
        assert_eq!(
            self.outgoing.recv().await,
            Some(ServerMessage::CommandAck { command_id: id })
        );
    }

    pub(super) async fn no_output(&mut self, fixture: &Fixture) {
        fixture.state.hub.barrier().await;
        assert!(matches!(
            self.outgoing.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            self.closing.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }
}

pub(super) fn result(command_id: Uuid, status: CommandStatus) -> ClientMessage {
    ClientMessage::CommandResult {
        command_id,
        status,
        error_code: (status == CommandStatus::Failed)
            .then(|| error_code::RECORDING_FINISH_FAILED.to_owned()),
    }
}
