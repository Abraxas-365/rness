//! M1 exit criteria: headless turns against a scripted fake provider,
//! fully committed to the log and replayable — including tool round
//! trips, mid-turn steering, retryable failures, and cancellation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rness_engine::inbox::Pending;
use rness_engine::session::branch::SessionStore;
use rness_engine::session::replay::replay;
use rness_engine::tools::{Tool, ToolRegistry};
use rness_engine::turn::provider::{Provider, ProviderError, StepOutcome, StepRequest};
use rness_engine::turn::{run_turn, TurnConfig, TurnError};
use rness_protocol::events::*;
use tokio_util::sync::CancellationToken;

// -- scripted provider -----------------------------------------------------

/// Plays a fixed sequence of step outcomes; records the contexts it saw.
struct Scripted {
    steps: Mutex<Vec<StepOutcome>>,
    seen_contexts: Mutex<Vec<usize>>, // turn counts per request, for steering asserts
    seen_texts: Mutex<Vec<String>>,   // flattened user text per request
}

impl Scripted {
    fn new(steps: Vec<StepOutcome>) -> Self {
        Self {
            steps: Mutex::new(steps),
            seen_contexts: Mutex::new(vec![]),
            seen_texts: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl Provider for Scripted {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, request: StepRequest<'_>, _cancel: &CancellationToken) -> StepOutcome {
        let context = request.context;
        self.seen_contexts.lock().unwrap().push(context.turns.len());
        let texts: Vec<String> = context
            .turns
            .iter()
            .filter_map(|t| match t {
                rness_engine::session::projection::ModelTurn::User { content } => {
                    content.iter().find_map(|p| match p {
                        ContentPart::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                }
                _ => None,
            })
            .collect();
        self.seen_texts.lock().unwrap().push(texts.join("|"));
        self.steps.lock().unwrap().remove(0)
    }
}

fn assistant(text: &str, stop: StopReason, tool_calls: Vec<(&str, &str)>) -> AssistantMessage {
    let mut content = vec![ContentPart::Text { text: text.into() }];
    for (call, name) in tool_calls {
        content.push(ContentPart::ToolUse {
            call: call.into(),
            name: name.into(),
            args: serde_json::json!({}),
        });
    }
    AssistantMessage {
        model: "fake-1".into(),
        content,
        stop,
        usage: Usage { input_tokens: 1, output_tokens: 1, ..Default::default() },
        chunks: vec![TimedChunk { ms: 1, delta: ChunkDelta::Text { t: text.into() } }],
    }
}

struct Echo;
#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "Echo"
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<String, String> {
        Ok("echo-output".into())
    }
}

fn no_steers() -> impl FnMut() -> Vec<Pending> + Send {
    Vec::new
}

// -- tests -----------------------------------------------------------------

struct RestrictedRequest;

#[async_trait]
impl Provider for RestrictedRequest {
    fn model(&self) -> &str { "restricted-test" }
    async fn step(&self, request: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
        assert!(request.system.ends_with("Only review"));
        assert!(request.tools.is_empty());
        StepOutcome::Committed(assistant("try forbidden", StopReason::ToolUse, vec![("denied", "Echo")]))
    }
}

#[tokio::test]
async fn agent_instructions_and_ceiling_apply_to_schema_and_dispatch() {
    for ceiling in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut log = store.create(None).unwrap();
        log.append(&SessionEvent::RequestConfig(CallConfig {
            tool_ceiling: ceiling.then(Vec::new),
            agent: Some(AgentSnapshot {
                name: "reviewer".into(), instructions: "Only review".into(),
                tools: if ceiling { None } else { Some(vec![]) },
            }),
            ..Default::default()
        })).unwrap();
        log.append(&SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup, content: vec![ContentPart::Text { text: "review".into() }], source: None,
        })).unwrap();
        let tools = ToolRegistry::default();
        tools.register(Arc::new(Echo));
        run_turn(&store, &mut log, &RestrictedRequest, &tools,
            &TurnConfig { max_steps: 1, ..Default::default() },
            &CancellationToken::new(), &mut no_steers(), 1, &|_| {}).await.unwrap();
        let history = store.history(log.session()).unwrap();
        assert!(history.iter().any(|event| matches!(&event.event, SessionEvent::ToolResult(result) if result.is_error)));
    }
}

#[tokio::test]
async fn tool_roundtrip_turn_commits_and_replays() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    log.append(&SessionEvent::UserMessage(UserMessage {
        intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: "run the tool".into() }],
        source: None,
    }))
    .unwrap();

    let provider = Scripted::new(vec![
        StepOutcome::Committed(assistant("calling", StopReason::ToolUse, vec![("c1", "Echo")])),
        StepOutcome::Committed(assistant("done", StopReason::EndTurn, vec![])),
    ]);
    let mut tools = ToolRegistry::default();
    tools.register(Arc::new(Echo));

    let mut steers = no_steers();
    let outcome = run_turn(
        &store,
        &mut log,
        &provider,
        &tools,
        &TurnConfig::default(),
        &CancellationToken::new(),
        &mut steers,
        1,
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome, TurnOutcome::Completed);
    drop(log);

    // The log tells the whole story, in order.
    let replayed = replay(&store, &sid).unwrap();
    let kinds: Vec<&str> = replayed
        .history
        .iter()
        .map(|e| match &e.event {
            SessionEvent::Header(_) => "header",
            SessionEvent::UserMessage(_) => "user",
            SessionEvent::TurnStarted { .. } => "turn+",
            SessionEvent::AssistantMessage(_) => "assistant",
            SessionEvent::ToolResult(_) => "tool",
            SessionEvent::TurnEnded { .. } => "turn-",
            SessionEvent::AssistantAttempt(_) => "attempt",
            SessionEvent::Compaction(_) => "compaction",
            SessionEvent::Prune(_) => "prune",
            SessionEvent::RequestConfig(_) => "config",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["header", "user", "turn+", "assistant", "tool", "assistant", "turn-"]
    );
    // Second request saw: user, assistant, tool_results (3 turns).
    assert_eq!(*provider.seen_contexts.lock().unwrap(), vec![1, 3]);
    // Model context includes the tool result round trip.
    assert_eq!(replayed.context.turns.len(), 4);
}

#[tokio::test]
async fn steer_lands_before_next_step() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    log.append(&SessionEvent::UserMessage(UserMessage {
        intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: "start".into() }],
        source: None,
    }))
    .unwrap();

    let provider = Scripted::new(vec![
        StepOutcome::Committed(assistant("step1", StopReason::ToolUse, vec![("c1", "Echo")])),
        StepOutcome::Committed(assistant("step2", StopReason::EndTurn, vec![])),
    ]);
    let mut tools = ToolRegistry::default();
    tools.register(Arc::new(Echo));

    // A steer arrives after the first step has been consumed.
    let calls = AtomicUsize::new(0);
    let mut steers = move || {
        if calls.fetch_add(1, Ordering::Relaxed) == 1 {
            vec![Pending {
                intent: UserIntent::Steer,
                content: vec![ContentPart::Text { text: "actually stop".into() }],
            }]
        } else {
            vec![]
        }
    };

    run_turn(
        &store,
        &mut log,
        &provider,
        &tools,
        &TurnConfig::default(),
        &CancellationToken::new(),
        &mut steers,
        1,
        &|_| {},
    )
    .await
    .unwrap();

    // First request: no steer. Second request: steer text present.
    let seen = provider.seen_texts.lock().unwrap();
    assert_eq!(seen[0], "start");
    assert_eq!(seen[1], "start|actually stop");
}

#[tokio::test]
async fn retryable_failure_becomes_attempt_then_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    log.append(&SessionEvent::UserMessage(UserMessage {
        intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: "go".into() }],
        source: None,
    }))
    .unwrap();

    let provider = Scripted::new(vec![
        StepOutcome::Failed {
            error: ProviderError { message: "overloaded".into(), retryable: true },
            partial: vec![TimedChunk { ms: 5, delta: ChunkDelta::Text { t: "par".into() } }],
        },
        StepOutcome::Committed(assistant("ok", StopReason::EndTurn, vec![])),
    ]);
    let mut steers = no_steers();
    let outcome = run_turn(
        &store,
        &mut log,
        &provider,
        &ToolRegistry::default(),
        &TurnConfig::default(),
        &CancellationToken::new(),
        &mut steers,
        1,
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome, TurnOutcome::Completed);
    drop(log);

    let replayed = replay(&store, &sid).unwrap();
    // Attempt preserved in history, absent from model context.
    assert!(replayed
        .history
        .iter()
        .any(|e| matches!(e.event, SessionEvent::AssistantAttempt(_))));
    assert_eq!(replayed.context.turns.len(), 2); // user + committed assistant
}

#[tokio::test]
async fn non_retryable_failure_fails_turn_but_commits_trace() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    log.append(&SessionEvent::UserMessage(UserMessage {
        intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: "go".into() }],
        source: None,
    }))
    .unwrap();

    let provider = Scripted::new(vec![StepOutcome::Failed {
        error: ProviderError { message: "invalid api key".into(), retryable: false },
        partial: vec![],
    }]);
    let mut steers = no_steers();
    let result = run_turn(
        &store,
        &mut log,
        &provider,
        &ToolRegistry::default(),
        &TurnConfig::default(),
        &CancellationToken::new(),
        &mut steers,
        1,
        &|_| {},
    )
    .await;
    assert!(matches!(result, Err(TurnError::ModelExhausted { attempts: 1, .. })));
    drop(log);

    // turn/ended(failed) + the attempt are in the log.
    let history = replay(&store, &sid).unwrap().history;
    assert!(history.iter().any(|e| matches!(
        e.event,
        SessionEvent::TurnEnded { outcome: TurnOutcome::Failed, .. }
    )));
}

#[tokio::test]
async fn cancellation_mid_stream_preserves_partial() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    log.append(&SessionEvent::UserMessage(UserMessage {
        intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: "go".into() }],
        source: None,
    }))
    .unwrap();

    let provider = Scripted::new(vec![StepOutcome::Cancelled {
        partial: vec![TimedChunk { ms: 9, delta: ChunkDelta::Text { t: "half a tho".into() } }],
    }]);
    let mut steers = no_steers();
    let outcome = run_turn(
        &store,
        &mut log,
        &provider,
        &ToolRegistry::default(),
        &TurnConfig::default(),
        &CancellationToken::new(),
        &mut steers,
        1,
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome, TurnOutcome::Cancelled);
    drop(log);

    let replayed = replay(&store, &sid).unwrap();
    let attempt = replayed
        .history
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::AssistantAttempt(a) => Some(a),
            _ => None,
        })
        .expect("cancelled attempt preserved");
    assert_eq!(attempt.outcome, AttemptOutcome::Cancelled);
    assert_eq!(attempt.chunks.len(), 1);
    // Model context untouched by the dead stream.
    assert_eq!(replayed.context.turns.len(), 1);
}
