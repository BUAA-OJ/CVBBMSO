//! Property tests for wire round-trips and validation invariants.

use cvbbmso_protocol::{
    AccodingUser, ClientHello, ClientIncident, ClientMessage, ClientState, CommandName,
    CommandStatus, ExamEvent, ExamListResponse, ExamPhase, ExamSummary, ExecutionState,
    IncidentCode, Platform, RecordingManifest, RecordingPolicy, RecordingState, SegmentMeta,
    ServerMessage, Sha256Digest,
};
use proptest::prelude::*;
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

fn arb_time() -> impl Strategy<Value = OffsetDateTime> {
    (0_i64..4_102_444_800).prop_map(|seconds| {
        OffsetDateTime::from_unix_timestamp(seconds).expect("timestamp within the supported range")
    })
}

fn arb_ordered_times() -> impl Strategy<Value = (OffsetDateTime, OffsetDateTime)> {
    (arb_time(), arb_time()).prop_map(|(first, second)| {
        if first <= second {
            (first, second)
        } else {
            (second, first)
        }
    })
}

fn arb_digest() -> impl Strategy<Value = Sha256Digest> {
    "[0-9a-f]{64}".prop_map(|value| Sha256Digest::parse(&value).expect("regex yields a digest"))
}

fn arb_uuid() -> impl Strategy<Value = Uuid> {
    any::<[u8; 16]>().prop_map(Uuid::from_bytes)
}

fn arb_platform() -> impl Strategy<Value = Platform> {
    prop_oneof![Just(Platform::Windows), Just(Platform::Macos)]
}

fn arb_client_state() -> impl Strategy<Value = ClientState> {
    prop_oneof![
        (Just(ExecutionState::Ready), Just(RecordingState::Idle)),
        (
            Just(ExecutionState::Recording),
            Just(RecordingState::Capturing)
        ),
        (
            Just(ExecutionState::Recording),
            Just(RecordingState::Failed)
        ),
        (
            Just(ExecutionState::Finalizing),
            Just(RecordingState::Finalizing)
        ),
        (
            Just(ExecutionState::Finalizing),
            Just(RecordingState::Complete)
        ),
        (
            Just(ExecutionState::Finalizing),
            Just(RecordingState::Failed)
        ),
        (
            Just(ExecutionState::Finished),
            Just(RecordingState::Complete)
        ),
        (Just(ExecutionState::Finished), Just(RecordingState::Failed)),
    ]
    .prop_map(|(execution, recording)| ClientState {
        execution,
        recording,
    })
}

fn arb_phase() -> impl Strategy<Value = ExamPhase> {
    prop_oneof![
        Just(ExamPhase::Scheduled),
        Just(ExamPhase::Running),
        Just(ExamPhase::Ended),
    ]
}

fn arb_policy() -> impl Strategy<Value = RecordingPolicy> {
    (1_u32..3_600, 1_u32..60).prop_map(|(segment_duration_secs, fps)| {
        RecordingPolicy::new(segment_duration_secs, fps).expect("both values are positive")
    })
}

fn arb_segment() -> impl Strategy<Value = SegmentMeta> {
    (1_u32..10_000, arb_ordered_times(), arb_digest()).prop_map(
        |(index, (start_at, end_at), sha256)| {
            SegmentMeta::new(index, start_at, end_at, sha256).expect("valid segment")
        },
    )
}

fn arb_manifest() -> impl Strategy<Value = RecordingManifest> {
    (
        prop::collection::vec((arb_ordered_times(), arb_digest()), 1..4),
        arb_digest(),
    )
        .prop_map(|(parts, merged_sha256)| {
            let segments = parts
                .into_iter()
                .enumerate()
                .map(|(offset, ((start_at, end_at), sha256))| {
                    let index = u32::try_from(offset).expect("offset fits in u32") + 1;
                    SegmentMeta::new(index, start_at, end_at, sha256).expect("valid segment")
                })
                .collect();
            RecordingManifest::new(segments, merged_sha256).expect("indices are ascending")
        })
}

fn arb_incident_code() -> impl Strategy<Value = IncidentCode> {
    prop_oneof![
        Just(IncidentCode::RecordingFailed),
        Just(IncidentCode::RecordingRecovered),
        Just(IncidentCode::NavigationDenied),
    ]
}

fn arb_incident() -> impl Strategy<Value = ClientIncident> {
    (
        arb_incident_code(),
        arb_time(),
        prop::option::of("[ -~]{0,32}"),
    )
        .prop_map(|(code, occurred_at, detail)| ClientIncident {
            code,
            occurred_at,
            detail,
        })
}

fn arb_event() -> impl Strategy<Value = ExamEvent> {
    prop_oneof![
        arb_segment().prop_map(ExamEvent::SegmentFinalized),
        arb_manifest().prop_map(ExamEvent::RecordingFinalized),
        arb_incident().prop_map(ExamEvent::Incident),
    ]
}

fn arb_command_name() -> impl Strategy<Value = CommandName> {
    prop_oneof![Just(CommandName::StartExam), Just(CommandName::EndExam)]
}

fn arb_command_status() -> impl Strategy<Value = CommandStatus> {
    prop_oneof![Just(CommandStatus::Completed), Just(CommandStatus::Failed)]
}

fn arb_client_message() -> impl Strategy<Value = ClientMessage> {
    prop_oneof![
        (arb_uuid(), arb_uuid(), arb_platform(), arb_client_state()).prop_map(
            |(device_id, instance_id, platform, state)| {
                ClientMessage::Hello(ClientHello {
                    device_id,
                    instance_id,
                    platform,
                    state,
                })
            }
        ),
        Just(ClientMessage::Heartbeat),
        arb_client_state().prop_map(|state| ClientMessage::Status { state }),
        (any::<u64>(), arb_event()).prop_map(|(seq, event)| ClientMessage::Event { seq, event }),
        (
            arb_uuid(),
            arb_command_status(),
            prop::option::of("[A-Z_]{1,32}")
        )
            .prop_map(|(command_id, status, error_code)| {
                ClientMessage::CommandResult {
                    command_id,
                    status,
                    error_code,
                }
            }),
    ]
}

fn arb_server_message() -> impl Strategy<Value = ServerMessage> {
    prop_oneof![
        (
            arb_time(),
            arb_phase(),
            arb_ordered_times(),
            arb_policy(),
            any::<u64>()
        )
            .prop_map(
                |(server_time, phase, (start_at, end_at), recording_policy, ack)| {
                    ServerMessage::Sync {
                        server_time,
                        phase,
                        start_at,
                        end_at,
                        recording_policy,
                        ack_event_through: ack,
                    }
                }
            ),
        (arb_uuid(), arb_command_name(), arb_time()).prop_map(
            |(command_id, name, effective_at)| ServerMessage::Command {
                command_id,
                name,
                effective_at,
            }
        ),
        arb_uuid().prop_map(|command_id| ServerMessage::CommandAck { command_id }),
        ("[A-Z_]{1,32}", prop::option::of("[ -~]{0,32}")).prop_map(|(code, message)| {
            ServerMessage::Error {
                code,
                message: message.unwrap_or_default(),
            }
        }),
    ]
}

fn arb_exam_list() -> impl Strategy<Value = ExamListResponse> {
    (
        1_i64..1_000_000_000,
        "[ -~]{0,32}",
        "[ -~]{0,32}",
        prop::collection::vec(
            (arb_uuid(), "[ -~]{0,32}", arb_ordered_times(), arb_phase()),
            0..4,
        ),
    )
        .prop_map(|(id, username, nickname, items)| ExamListResponse {
            user: AccodingUser::new(id, username, nickname).expect("id is positive"),
            items: items
                .into_iter()
                .map(|(exam_id, title, (start_at, end_at), phase)| {
                    ExamSummary::new(exam_id, title, start_at, end_at, phase)
                        .expect("times are ordered")
                })
                .collect(),
        })
}

proptest! {
    #[test]
    fn client_messages_round_trip(message in arb_client_message()) {
        let json = serde_json::to_string(&message).expect("serialize client message");
        let decoded: ClientMessage =
            serde_json::from_str(&json).expect("deserialize client message");
        prop_assert_eq!(message, decoded);
    }

    #[test]
    fn server_messages_round_trip(message in arb_server_message()) {
        let json = serde_json::to_string(&message).expect("serialize server message");
        let decoded: ServerMessage =
            serde_json::from_str(&json).expect("deserialize server message");
        prop_assert_eq!(message, decoded);
    }

    #[test]
    fn exam_list_round_trips(response in arb_exam_list()) {
        let json = serde_json::to_string(&response).expect("serialize exam list");
        let decoded: ExamListResponse =
            serde_json::from_str(&json).expect("deserialize exam list");
        prop_assert_eq!(response, decoded);
    }

    #[test]
    fn accepted_digests_are_lowercase_hex(value in "\\PC{0,80}") {
        if let Ok(digest) = Sha256Digest::parse(&value) {
            prop_assert_eq!(digest.as_str().len(), 64);
            prop_assert!(
                digest
                    .as_str()
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            );
        }
    }

    #[test]
    fn timestamps_serialize_in_utc(index in 1_u32..100, (start, end) in arb_ordered_times(), digest in arb_digest()) {
        let offset = UtcOffset::from_hms(8, 0, 0).expect("valid offset");
        let segment = SegmentMeta::new(index, start.to_offset(offset), end.to_offset(offset), digest)
            .expect("valid segment");
        let json = serde_json::to_value(&segment).expect("serialize segment");
        prop_assert!(json["start_at"].as_str().is_some_and(|value| value.ends_with('Z')));
        prop_assert!(json["end_at"].as_str().is_some_and(|value| value.ends_with('Z')));
    }
}
