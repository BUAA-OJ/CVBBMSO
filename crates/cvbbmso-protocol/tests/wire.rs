//! Wire-format tests: the documented JSON shape must round-trip and stay
//! visible in snapshots.

use cvbbmso_protocol::{
    AccodingUser, ClientHello, ClientIncident, ClientMessage, ClientState, CommandName,
    CommandStatus, ExamEvent, ExamListResponse, ExamPhase, ExamSummary, ExecutionState,
    IncidentCode, Platform, RecordingPolicy, RecordingState, RecordingSummary, SegmentMeta,
    ServerMessage, Sha256Digest,
};
use time::OffsetDateTime;
use uuid::Uuid;

fn at(value: &str) -> OffsetDateTime {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .expect("valid timestamp")
}

fn digest(fill: char) -> Sha256Digest {
    Sha256Digest::parse(&fill.to_string().repeat(64)).expect("valid digest")
}

fn segment(index: u32, start: &str, end: &str, fill: char) -> SegmentMeta {
    SegmentMeta::new(index, at(start), at(end), digest(fill)).expect("valid segment")
}

fn state() -> ClientState {
    ClientState {
        execution: ExecutionState::Recording,
        recording: RecordingState::Capturing,
    }
}

fn command_result(status: CommandStatus, error_code: Option<&str>) -> ClientMessage {
    ClientMessage::CommandResult {
        command_id: Uuid::from_u128(0x40),
        status,
        error_code: error_code.map(str::to_owned),
    }
}

#[test]
fn hello_message_matches_the_wire_format() {
    let message = ClientMessage::Hello(ClientHello {
        device_id: Uuid::from_u128(0x20),
        instance_id: Uuid::from_u128(0x30),
        platform: Platform::Windows,
        state: ClientState {
            execution: ExecutionState::Ready,
            recording: RecordingState::Idle,
        },
    });

    insta::assert_json_snapshot!("hello", message);
}

#[test]
fn server_messages_match_the_wire_format() {
    insta::assert_json_snapshot!(
        "server_messages",
        vec![
            ServerMessage::Sync {
                server_time: at("2026-12-20T00:59:55Z"),
                phase: ExamPhase::Scheduled,
                start_at: at("2026-12-20T01:00:00Z"),
                end_at: at("2026-12-20T04:00:00Z"),
                recording_policy: RecordingPolicy::new(30, 15).expect("valid policy"),
                ack_event_through: 3,
            },
            ServerMessage::Command {
                command_id: Uuid::from_u128(0x40),
                name: CommandName::StartExam,
                effective_at: at("2026-12-20T01:00:00Z"),
            },
            ServerMessage::CommandAck {
                command_id: Uuid::from_u128(0x40),
            },
            ServerMessage::Error {
                code: "PROTOCOL_ERROR".to_owned(),
                message: "hello must be the first message".to_owned(),
            },
        ]
    );
}

#[test]
fn client_messages_match_the_wire_format() {
    insta::assert_json_snapshot!(
        "client_messages",
        vec![
            ClientMessage::Heartbeat,
            ClientMessage::Status { state: state() },
            ClientMessage::Event {
                seq: 4,
                event: ExamEvent::SegmentFinalized(segment(
                    1,
                    "2026-12-20T01:00:00Z",
                    "2026-12-20T01:00:30Z",
                    'a',
                )),
            },
            ClientMessage::Event {
                seq: 5,
                event: ExamEvent::Incident(ClientIncident {
                    code: IncidentCode::RecordingFailed,
                    occurred_at: at("2026-12-20T01:00:31Z"),
                    detail: Some("capture stream stopped".to_owned()),
                }),
            },
            ClientMessage::Event {
                seq: 6,
                event: ExamEvent::RecordingFinalized(
                    RecordingSummary::new(2, digest('c')).expect("valid summary"),
                ),
            },
            command_result(CommandStatus::Completed, None),
            command_result(CommandStatus::Failed, Some("RECORDING_START_FAILED")),
        ]
    );
}

#[test]
fn recording_finalized_has_a_bounded_and_strict_payload() {
    let message = ClientMessage::Event {
        seq: i64::MAX,
        event: ExamEvent::RecordingFinalized(RecordingSummary::new(u32::MAX, digest('c')).unwrap()),
    };
    let encoded = serde_json::to_string(&message).unwrap();
    assert_eq!(encoded.len(), 200);
    assert_eq!(
        serde_json::from_str::<ClientMessage>(&encoded).unwrap(),
        message
    );
    assert!(RecordingSummary::new(0, digest('c')).is_err());

    let json = serde_json::to_value(&message).unwrap();
    for (field, value) in [
        ("segment_count", serde_json::json!(0)),
        ("segment_count", serde_json::json!(-1)),
        ("segment_count", serde_json::json!(u64::from(u32::MAX) + 1)),
        ("merged_sha256", serde_json::json!("not-a-digest")),
        ("segments", serde_json::json!([])),
    ] {
        let mut invalid = json.clone();
        invalid["event"]["data"][field] = value;
        assert!(serde_json::from_value::<ClientMessage>(invalid).is_err());
    }
}

#[test]
fn exam_list_matches_the_wire_format() {
    let response = ExamListResponse {
        user: AccodingUser::new(12345, "user@example.com".to_owned(), "示例用户".to_owned())
            .expect("valid user"),
        items: vec![
            ExamSummary::new(
                Uuid::from_u128(0x10),
                "程序设计期末考试".to_owned(),
                at("2026-12-20T01:00:00Z"),
                at("2026-12-20T04:00:00Z"),
                ExamPhase::Scheduled,
            )
            .expect("valid summary"),
        ],
    };

    insta::assert_json_snapshot!("exam_list", response);
}

#[test]
fn documented_client_messages_deserialize() {
    let hello: ClientMessage = serde_json::from_str(
        r#"{"type":"hello","device_id":"00000000-0000-0000-0000-000000000020","instance_id":"00000000-0000-0000-0000-000000000030","platform":"windows","state":{"execution":"ready","recording":"idle"}}"#,
    )
    .expect("hello parses");
    assert!(matches!(hello, ClientMessage::Hello(_)));

    let event: ClientMessage = serde_json::from_str(
        r#"{"type":"event","seq":4,"event":{"kind":"segment_finalized","data":{"index":1,"start_at":"2026-12-20T01:00:00Z","end_at":"2026-12-20T01:00:30Z","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}"#,
    )
    .expect("event parses");
    assert!(matches!(
        event,
        ClientMessage::Event {
            seq: 4,
            event: ExamEvent::SegmentFinalized(_)
        }
    ));

    let result: ClientMessage = serde_json::from_str(
        r#"{"type":"command_result","command_id":"00000000-0000-0000-0000-000000000040","status":"completed"}"#,
    )
    .expect("command result parses");
    assert!(matches!(
        result,
        ClientMessage::CommandResult {
            status: CommandStatus::Completed,
            error_code: None,
            ..
        }
    ));
}

#[test]
fn documented_server_messages_deserialize() {
    let sync: ServerMessage = serde_json::from_str(
        r#"{"type":"sync","server_time":"2026-12-20T00:59:55Z","phase":"scheduled","start_at":"2026-12-20T01:00:00Z","end_at":"2026-12-20T04:00:00Z","recording_policy":{"segment_duration_secs":30,"fps":15},"ack_event_through":3}"#,
    )
    .expect("sync parses");
    assert!(matches!(
        sync,
        ServerMessage::Sync {
            phase: ExamPhase::Scheduled,
            ..
        }
    ));

    let command: ServerMessage = serde_json::from_str(
        r#"{"type":"command","command_id":"00000000-0000-0000-0000-000000000040","name":"start_exam","effective_at":"2026-12-20T01:00:00Z"}"#,
    )
    .expect("command parses");
    assert!(matches!(
        command,
        ServerMessage::Command {
            name: CommandName::StartExam,
            ..
        }
    ));

    let ack: ServerMessage = serde_json::from_str(
        r#"{"type":"command_ack","command_id":"00000000-0000-0000-0000-000000000040"}"#,
    )
    .expect("command ack parses");
    assert!(matches!(
        ack,
        ServerMessage::CommandAck { command_id } if command_id == Uuid::from_u128(0x40)
    ));

    let error: ServerMessage = serde_json::from_str(
        r#"{"type":"error","code":"PROTOCOL_ERROR","message":"hello must be the first message"}"#,
    )
    .expect("error parses");
    assert!(matches!(error, ServerMessage::Error { .. }));
}

#[test]
fn event_sequences_and_acknowledgments_have_distinct_lower_bounds() {
    let event = |seq| {
        format!(
            r#"{{"type":"event","seq":{seq},"event":{{"kind":"incident","data":{{"code":"recording_failed","occurred_at":"2026-12-20T00:59:55Z"}}}}}}"#
        )
    };
    let sync = |ack| {
        format!(
            r#"{{"type":"sync","server_time":"2026-12-20T00:59:55Z","phase":"scheduled","start_at":"2026-12-20T01:00:00Z","end_at":"2026-12-20T04:00:00Z","recording_policy":{{"segment_duration_secs":30,"fps":15}},"ack_event_through":{ack}}}"#
        )
    };
    for invalid in ["-1", "0", "9223372036854775808"] {
        assert!(serde_json::from_str::<ClientMessage>(&event(invalid)).is_err());
    }
    for invalid in ["-1", "9223372036854775808"] {
        assert!(serde_json::from_str::<ServerMessage>(&sync(invalid)).is_err());
    }
    assert!(serde_json::from_str::<ClientMessage>(&event("9223372036854775807")).is_ok());
    assert!(serde_json::from_str::<ServerMessage>(&sync("9223372036854775807")).is_ok());
    assert!(serde_json::from_str::<ServerMessage>(&sync("0")).is_ok());
}
