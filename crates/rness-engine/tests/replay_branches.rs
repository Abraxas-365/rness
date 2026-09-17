//! M1b integration: a realistic session (turn, assistant with tool call,
//! failed attempt, retry, fork) — projections derive the right model
//! context and transcript on both branches, invariant-checked replay.

use rness_engine::session::branch::SessionStore;
use rness_engine::session::projection::{transcript, ModelTurn, TranscriptItem};
use rness_engine::session::replay::replay;
use rness_protocol::events::*;

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserMessage(UserMessage {
        intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: text.into() }],
        source: None,
    })
}

fn assistant_tool_use() -> SessionEvent {
    SessionEvent::AssistantMessage(AssistantMessage {
        model: "m1".into(),
        content: vec![
            ContentPart::Text { text: "reading the file".into() },
            ContentPart::ToolUse {
                call: "c1".into(),
                name: "Read".into(),
                args: serde_json::json!({"path": "/etc/hosts"}),
            },
        ],
        stop: StopReason::ToolUse,
        usage: Usage { input_tokens: 100, output_tokens: 20, ..Default::default() },
        estimated_input: 0,
        chunks: vec![TimedChunk { ms: 90, delta: ChunkDelta::Text { t: "reading the file".into() } }],
    })
}

fn assistant_final(text: &str) -> SessionEvent {
    SessionEvent::AssistantMessage(AssistantMessage {
        model: "m1".into(),
        content: vec![ContentPart::Text { text: text.into() }],
        stop: StopReason::EndTurn,
        usage: Usage { input_tokens: 150, output_tokens: 10, ..Default::default() },
        estimated_input: 0,
        chunks: vec![],
    })
}

#[test]
fn full_turn_with_attempt_projects_and_replays() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(Some("/w".into())).unwrap();
    let sid = log.session().clone();

    log.append(&SessionEvent::TurnStarted { turn: 1 }).unwrap();
    log.append(&user("what's in /etc/hosts?")).unwrap();

    // First model call dies (overloaded) — preserved as an attempt.
    log.append(&SessionEvent::AssistantAttempt(AssistantAttempt {
        model: "m1".into(),
        outcome: AttemptOutcome::Error { code: None, retry_in_ms: None, message: "overloaded".into(), retryable: true },
        chunks: vec![TimedChunk { ms: 10, delta: ChunkDelta::Text { t: "wha".into() } }],
    }))
    .unwrap();

    // Retry succeeds with a tool call, tool result commits, final answer.
    log.append(&assistant_tool_use()).unwrap();
    log.append(&SessionEvent::ToolResult(ToolResult {
        content: vec![], tasks: None, plan_review: None, presentation: Some(serde_json::json!({
            "version":1,"kind":"read","text":"127.0.0.1 localhost","start_line":1,
        })),
        call: "c1".into(),
        name: "Read".into(),
        output: "127.0.0.1 localhost".into(),
        is_error: false,
        duration_ms: 3,
    }))
    .unwrap();
    let final_msg = log.append(&assistant_final("it maps localhost")).unwrap();
    log.append(&SessionEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Completed }).unwrap();
    drop(log);

    // Reopen through a fresh store, not an in-memory event cache.
    let store = SessionStore::new(dir.path());
    let replayed = replay(&store, &sid).unwrap();
    let result = replayed.history.iter().find_map(|env| match &env.event {
        SessionEvent::ToolResult(result) => Some(result),
        _ => None,
    }).unwrap();
    assert_eq!(result.presentation.as_ref().unwrap()["text"], "127.0.0.1 localhost");
    assert_eq!(result.output, "127.0.0.1 localhost");

    // Model context: user -> assistant(tool_use) -> tool_results -> assistant.
    // The attempt is NOT there; turn markers are NOT there.
    assert_eq!(replayed.context.turns.len(), 4);
    assert!(matches!(replayed.context.turns[0], ModelTurn::User { .. }));
    assert!(matches!(replayed.context.turns[1], ModelTurn::Assistant { .. }));
    match &replayed.context.turns[2] {
        ModelTurn::ToolResults { results } => {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].call, "c1");
        }
        other => panic!("expected tool results, got {other:?}"),
    }
    assert!(matches!(replayed.context.turns[3], ModelTurn::Assistant { .. }));
    assert_eq!(replayed.context.usage.input_tokens, 250);
    assert_eq!(replayed.context.usage.output_tokens, 30);

    // Transcript: the attempt IS visible to frontends.
    let t = transcript(&replayed.history);
    assert!(t.items.iter().any(|i| matches!(i, TranscriptItem::Attempt { .. })));
    assert_eq!(t.items.len(), 5); // user, attempt, assistant, tool, assistant

    // Fork at the tool-use assistant message: context on the branch stops there.
    let tool_use_event = replayed
        .history
        .iter()
        .find(|e| matches!(&e.event, SessionEvent::AssistantMessage(m) if m.stop == StopReason::ToolUse))
        .unwrap()
        .id
        .clone();
    let child = store.fork(&sid, Some(tool_use_event)).unwrap();
    let child_id = child.session().clone();
    drop(child);

    let branch = replay(&store, &child_id).unwrap();
    assert_eq!(branch.context.turns.len(), 2); // user + assistant(tool_use)
    // The final answer belongs only to the parent.
    assert!(!branch.history.iter().any(|e| e.id == final_msg.id));
    // Parent unaffected.
    assert_eq!(replay(&store, &sid).unwrap().context.turns.len(), 4);
}

#[test]
fn replay_repairs_orphan_tool_use_without_mutating_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    log.append(&user("write the report")).unwrap();
    log.append(&SessionEvent::AssistantMessage(AssistantMessage {
        model: "m1".into(),
        content: vec![
            ContentPart::Text { text: "Writing it now.".into() },
            ContentPart::ToolUse { call: "orphan".into(), name: "Write".into(), args: serde_json::json!({}) },
        ],
        // This reproduces a provider output-limit stop from the older turn
        // loop: the call was committed but never dispatched.
        stop: StopReason::MaxTokens,
        usage: Usage::default(),
        estimated_input: 0,
        chunks: vec![],
    })).unwrap();
    log.append(&user("continue")).unwrap();
    drop(log);

    let replayed = replay(&store, &sid).unwrap();
    assert_eq!(replayed.history.len(), 4, "the durable log stays intact");
    assert_eq!(replayed.context.turns.len(), 3);
    match &replayed.context.turns[1] {
        ModelTurn::Assistant { content } => {
            assert!(content.iter().any(|part| matches!(part, ContentPart::Text { text } if text == "Writing it now.")));
            assert!(!content.iter().any(|part| matches!(part, ContentPart::ToolUse { .. })));
        }
        other => panic!("expected repaired assistant turn, got {other:?}"),
    }
    assert!(matches!(&replayed.context.turns[2], ModelTurn::User { content }
        if matches!(&content[0], ContentPart::Text { text } if text == "continue")));
}

#[test]
fn steer_and_inject_intents_reach_model_context() {
    // All three intents are user/message events; the difference is WHEN the
    // turn loop lets them in, not whether they replay.
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    for intent in [UserIntent::Followup, UserIntent::Steer, UserIntent::Inject] {
        log.append(&SessionEvent::UserMessage(UserMessage {
            intent,
            content: vec![ContentPart::Text { text: format!("{intent:?}") }],
            source: None,
        }))
        .unwrap();
    }
    drop(log);
    let replayed = replay(&store, &sid).unwrap();
    assert_eq!(replayed.context.turns.len(), 3);
}
