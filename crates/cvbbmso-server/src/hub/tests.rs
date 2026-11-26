use std::time::Duration;

use axum::extract::ws::Message;
use cvbbmso_protocol::{ClientIncident, ClientMessage, ExamEvent, IncidentCode};
use futures_util::{StreamExt, stream};
use tokio::time;

use super::*;
use crate::socket::read_frames;

#[tokio::test]
async fn heartbeats_bypass_blocked_reliable_writes_and_renew_deadline() {
    let (requests, mut pending) = mpsc::channel(4);
    let (heartbeats, mut pending_hb) = mpsc::channel(4);
    let budget = WriteBudget::new(8, 1024 * 1024);
    let exam = Arc::new(ExamService::open("sqlite::memory:").await.unwrap());
    time::pause();
    let hub = Hub {
        requests,
        heartbeats,
        budget,
        exam,
        config: HubConfig::default(),
    };
    let connection = Connection {
        student: StudentKey {
            exam_id: Uuid::new_v4(),
            oj_user_id: 1,
        },
        id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
    };
    let start = Instant::now();
    let old_deadline = start + Duration::from_secs(1);
    let renewed_deadline = start + Duration::from_secs(25);
    let event = ClientMessage::Event {
        seq: 1,
        event: ExamEvent::Incident(ClientIncident {
            code: IncidentCode::NavigationDenied,
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            detail: None,
        }),
    };
    let event_text = serde_json::to_string(&event).unwrap();
    let heartbeat = serde_json::to_string(&ClientMessage::Heartbeat).unwrap();
    let reader = stream::iter([
        Ok(Message::Text(event_text.into())),
        Ok(Message::Text(heartbeat.into())),
    ])
    .chain(stream::pending());
    tokio::pin!(reader);

    let simulate_hub = async {
        // Receive the reliable event first, holding its completion pending.
        let Some(Request::Reliable {
            message, processed, ..
        }) = pending.recv().await
        else {
            panic!("expected reliable event");
        };
        assert_eq!(message, event);

        // Heartbeat arrives immediately on the fast path while the reliable write is in flight.
        let Some(HeartbeatMsg { ack, .. }) = pending_hb.recv().await else {
            panic!("expected fast-path heartbeat");
        };
        assert!(ack.send(Some(renewed_deadline)).is_ok());

        // Advance past old_deadline to verify heartbeat renewed the deadline even while event #1
        // had not finished committing.
        time::advance(Duration::from_secs(2)).await;
        assert!(!processed.is_closed());
        assert!(processed.send(None).is_ok());
    };

    time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            read_frames(
                &mut reader,
                &hub,
                connection,
                old_deadline,
                Duration::from_secs(25)
            ),
            simulate_hub
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn per_connection_backpressure_pauses_reading_without_heartbeat_timeout() {
    let (requests, mut pending) = mpsc::channel(8);
    let (heartbeats, _pending_hb) = mpsc::channel(4);
    let budget = WriteBudget::new(8, 1024 * 1024);
    let exam = Arc::new(ExamService::open("sqlite::memory:").await.unwrap());
    time::pause();
    let timeout = Duration::from_secs(5);
    let config = HubConfig {
        heartbeat_timeout: timeout,
        max_per_conn_pending: 2,
        ..HubConfig::default()
    };
    let hub = Hub {
        requests,
        heartbeats,
        budget,
        exam,
        config,
    };
    let connection = Connection {
        student: StudentKey {
            exam_id: Uuid::new_v4(),
            oj_user_id: 2,
        },
        id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
    };
    let event = |seq| {
        serde_json::to_string(&ClientMessage::Event {
            seq,
            event: ExamEvent::Incident(ClientIncident {
                code: IncidentCode::NavigationDenied,
                occurred_at: OffsetDateTime::UNIX_EPOCH,
                detail: None,
            }),
        })
        .unwrap()
    };
    let reader = stream::iter([
        Ok(Message::Text(event(1).into())),
        Ok(Message::Text(event(2).into())),
        Ok(Message::Text(event(3).into())),
    ])
    .chain(stream::pending());
    tokio::pin!(reader);

    let simulate_hub = async {
        let mut completions = Vec::new();
        for _ in 0..2 {
            let Some(Request::Reliable { processed, .. }) = pending.recv().await else {
                panic!("expected reliable event");
            };
            completions.push(processed);
        }
        // Wait longer than the heartbeat timeout while the connection is backpressured at cap=2.
        time::advance(timeout + Duration::from_secs(2)).await;
        assert!(pending.try_recv().is_err());

        // Complete the first request; backpressure clears and refreshes deadline, allowing #3 in.
        completions
            .remove(0)
            .send(Some(Instant::now() + timeout))
            .unwrap();
        let Some(Request::Reliable {
            message: ClientMessage::Event { seq: 3, .. },
            processed,
            ..
        }) = pending.recv().await
        else {
            panic!("expected third event after backpressure relief");
        };
        let _ = completions.pop().unwrap().send(None);
        let _ = processed.send(None);
    };

    time::timeout(Duration::from_secs(20), async {
        tokio::join!(
            read_frames(
                &mut reader,
                &hub,
                connection,
                Instant::now() + timeout,
                timeout
            ),
            simulate_hub
        );
    })
    .await
    .unwrap();
}
