//! Wire-format guard tests. The serialized shapes in these snapshots are
//! the v1 format contract — if one of these breaks, that is a FORMAT
//! CHANGE: bump FORMAT_VERSION and write a migration instead of editing
//! the expectation.

use rness_protocol::branch::ForkRef;
use rness_protocol::events::*;

fn roundtrip(ev: &SessionEvent) -> SessionEvent {
    let env = Envelope {
        id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        at: "2026-09-06T12:00:00.000Z".into(),
        event: ev.clone(),
    };
    let json = serde_json::to_string(&env).unwrap();
    let back: Envelope = serde_json::from_str(&json).unwrap();
    back.event
}

#[test]
fn image_content_roundtrips_without_inline_bytes() {
    let part = ContentPart::Image {
        attachment: ImageRef {
            id: "a".repeat(64),
            media_type: "image/png".into(),
            bytes: 100,
            width: 20,
            height: 10,
        },
    };
    let value = serde_json::to_value(&part).unwrap();
    assert_eq!(value["kind"], "image");
    assert!(value["attachment"].get("data").is_none());
    assert_eq!(serde_json::from_value::<ContentPart>(value).unwrap(), part);
}

#[test]
fn all_event_kinds_roundtrip() {
    let events = vec![
        SessionEvent::Header(Header {
            version: FORMAT_VERSION,
            session: "01S".into(),
            parent: Some(ForkRef {
                session: "01P".into(),
                at: "01E".into(),
            }),
            delegation: None,
            workspace: Some("/w".into()),
        }),
        SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Steer,
            content: vec![ContentPart::Text { text: "hi".into() }],
            source: None,
        }),
        SessionEvent::AssistantMessage(AssistantMessage {
            model: "test-model".into(),
            content: vec![
                ContentPart::Thinking {
                    text: "hmm".into(),
                    signature: None,
                },
                ContentPart::Text {
                    text: "hello".into(),
                },
                ContentPart::ToolUse {
                    call: "c1".into(),
                    name: "Read".into(),
                    args: serde_json::json!({"path": "/f"}),
                },
            ],
            stop: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            estimated_input: 12,
            chunks: vec![
                TimedChunk {
                    ms: 100,
                    delta: ChunkDelta::Thinking { t: "hmm".into() },
                },
                TimedChunk {
                    ms: 250,
                    delta: ChunkDelta::Text { t: "hello".into() },
                },
                TimedChunk {
                    ms: 300,
                    delta: ChunkDelta::ToolArgs {
                        call: "c1".into(),
                        t: "{\"path\"".into(),
                    },
                },
            ],
        }),
        SessionEvent::AssistantAttempt(AssistantAttempt {
            model: "test-model".into(),
            outcome: AttemptOutcome::Error {
                code: None,
                retry_in_ms: None,
                message: "overloaded".into(),
                retryable: true,
            },
            chunks: vec![TimedChunk {
                ms: 50,
                delta: ChunkDelta::Text { t: "par".into() },
            }],
        }),
        SessionEvent::AssistantAttempt(AssistantAttempt {
            model: "test-model".into(),
            outcome: AttemptOutcome::Cancelled,
            chunks: vec![],
        }),
        SessionEvent::ToolResult(ToolResult {
            content: vec![],
            tasks: None,
            plan_review: None,
            presentation: None,
            call: "c1".into(),
            name: "Read".into(),
            output: "contents".into(),
            is_error: false,
            duration_ms: 12,
        }),
        SessionEvent::TurnStarted { turn: 1 },
        SessionEvent::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Completed,
        },
        SessionEvent::RequestConfig(CallConfig {
            reasoning: Some(Reasoning::BudgetTokens { tokens: 8192 }),
            ..Default::default()
        }),
        SessionEvent::RequestConfig(CallConfig::default()),
    ];
    for ev in &events {
        assert_eq!(&roundtrip(ev), ev);
    }
}

#[test]
fn wire_shape_is_stable_v1() {
    // Exact byte-level expectations for representative lines. These ARE
    // the format; do not edit without a version bump.
    let env = Envelope {
        id: "01ID".into(),
        at: "2026-09-06T12:00:00.000Z".into(),
        event: SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text: "hi".into() }],
            source: None,
        }),
    };
    assert_eq!(
        serde_json::to_string(&env).unwrap(),
        r#"{"id":"01ID","at":"2026-09-06T12:00:00.000Z","type":"user/message","intent":"followup","content":[{"kind":"text","text":"hi"}]}"#
    );

    let header = Envelope {
        id: "01ID".into(),
        at: "2026-09-06T12:00:00.000Z".into(),
        event: SessionEvent::Header(Header {
            version: 1,
            session: "01S".into(),
            parent: None,
            delegation: None,
            workspace: None,
        }),
    };
    assert_eq!(
        serde_json::to_string(&header).unwrap(),
        r#"{"id":"01ID","at":"2026-09-06T12:00:00.000Z","type":"session/header","version":1,"session":"01S"}"#
    );
}

#[test]
fn reader_tolerates_unknown_fields() {
    // Forward tolerance: a v1 reader must accept lines that carry extra
    // fields added by a later minor writer.
    let line = r#"{"id":"01ID","at":"t","type":"turn/started","turn":3,"future_field":true}"#;
    let env: Envelope = serde_json::from_str(line).unwrap();
    assert_eq!(env.event, SessionEvent::TurnStarted { turn: 3 });
}

#[test]
fn known_types_cover_every_variant() {
    use rness_protocol::events::KNOWN_TYPES;
    let samples = [
        SessionEvent::TurnStarted { turn: 1 },
        SessionEvent::Repair(LogRepair {
            bytes: 3,
            lines: 1,
            sidecar: "quarantine-x.bin".into(),
            reason: "r".into(),
        }),
        SessionEvent::Title(SessionTitle {
            title: "t".into(),
            source: TitleSource::Fallback,
        }),
        SessionEvent::PlanMode { active: true },
        SessionEvent::ToolsActivated { names: vec![] },
    ];
    for event in &samples {
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["type"], event.kind());
        assert!(KNOWN_TYPES.contains(&event.kind()), "{}", event.kind());
        assert_eq!(&roundtrip(event), event);
    }
    // Every tag is distinct and none is parsed as Unknown.
    let mut tags = KNOWN_TYPES.to_vec();
    tags.sort_unstable();
    tags.dedup();
    assert_eq!(tags.len(), KNOWN_TYPES.len());
}

#[test]
fn unknown_type_reads_as_unknown_and_serializes_verbatim() {
    let line = r#"{"id":"01ID","at":"t","type":"future/event","nested":{"a":[1,"b"]},"n":1.5}"#;
    let env = Envelope::parse_line(line.as_bytes()).unwrap();
    let SessionEvent::Unknown(unknown) = &env.event else {
        panic!("expected Unknown, got {:?}", env.event);
    };
    assert_eq!(unknown.kind, "future/event");
    assert_eq!(env.event.kind(), "future/event");
    // Generic serde path agrees with parse_line.
    let via_serde: Envelope = serde_json::from_str(line).unwrap();
    assert_eq!(via_serde, env);
    let back: serde_json::Value = serde_json::to_value(&env).unwrap();
    assert_eq!(
        back,
        serde_json::from_str::<serde_json::Value>(line).unwrap()
    );
}

#[test]
fn known_type_with_bad_fields_is_still_an_error() {
    // Real corruption must not hide behind the Unknown fallback.
    for line in [
        r#"{"id":"01ID","at":"t","type":"turn/started","turn":"x"}"#,
        r#"{"id":"01ID","at":"t","type":"user/message"}"#,
        r#"{"at":"t","type":"future/event"}"#,
        r#"{"id":"01ID","at":"t"}"#,
        "not json",
    ] {
        assert!(Envelope::parse_line(line.as_bytes()).is_err(), "{line}");
        assert!(serde_json::from_str::<Envelope>(line).is_err(), "{line}");
    }
}
