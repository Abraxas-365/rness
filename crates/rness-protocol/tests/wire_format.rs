//! Wire-format guard tests. The serialized shapes in these snapshots are
//! the v1 format contract — if one of these breaks, that is a FORMAT
//! CHANGE: bump FORMAT_VERSION and write a migration instead of editing
//! the expectation.

use rness_protocol::branch::ForkRef;
use rness_protocol::events::*;

fn roundtrip(ev: &SessionEvent) -> SessionEvent {
    let env = Envelope { id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(), at: "2026-09-06T12:00:00.000Z".into(), event: ev.clone() };
    let json = serde_json::to_string(&env).unwrap();
    let back: Envelope = serde_json::from_str(&json).unwrap();
    back.event
}

#[test]
fn all_event_kinds_roundtrip() {
    let events = vec![
        SessionEvent::Header(Header {
            version: FORMAT_VERSION,
            session: "01S".into(),
            parent: Some(ForkRef { session: "01P".into(), at: "01E".into() }),
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
                ContentPart::Thinking { text: "hmm".into(), signature: None },
                ContentPart::Text { text: "hello".into() },
                ContentPart::ToolUse {
                    call: "c1".into(),
                    name: "Read".into(),
                    args: serde_json::json!({"path": "/f"}),
                },
            ],
            stop: StopReason::ToolUse,
            usage: Usage { input_tokens: 10, output_tokens: 5, ..Default::default() },
            chunks: vec![
                TimedChunk { ms: 100, delta: ChunkDelta::Thinking { t: "hmm".into() } },
                TimedChunk { ms: 250, delta: ChunkDelta::Text { t: "hello".into() } },
                TimedChunk { ms: 300, delta: ChunkDelta::ToolArgs { call: "c1".into(), t: "{\"path\"".into() } },
            ],
        }),
        SessionEvent::AssistantAttempt(AssistantAttempt {
            model: "test-model".into(),
            outcome: AttemptOutcome::Error { message: "overloaded".into(), retryable: true },
            chunks: vec![TimedChunk { ms: 50, delta: ChunkDelta::Text { t: "par".into() } }],
        }),
        SessionEvent::AssistantAttempt(AssistantAttempt {
            model: "test-model".into(),
            outcome: AttemptOutcome::Cancelled,
            chunks: vec![],
        }),
        SessionEvent::ToolResult(ToolResult {
            call: "c1".into(),
            name: "Read".into(),
            output: "contents".into(),
            is_error: false,
            duration_ms: 12,
        }),
        SessionEvent::TurnStarted { turn: 1 },
        SessionEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Completed },
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
