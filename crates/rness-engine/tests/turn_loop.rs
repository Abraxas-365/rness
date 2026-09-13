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

#[tokio::test]
async fn middle_region_preserves_neighbors_and_rejects_invalid_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    for text in ["before".into(), "middle ".repeat(3000), "after".into()] {
        log.append(&SessionEvent::UserMessage(UserMessage { intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text }], source: None })).unwrap();
    }
    let provider = Scripted::new(vec![StepOutcome::Committed(assistant("summary", StopReason::EndTurn, vec![]))]);
    assert!(rness_engine::turn::compaction::reduce_region(&store, &mut log, &provider, "", &[],
        &compact_policy(100000), false, Some(1..2), &CancellationToken::new()).await.unwrap());
    let context = replay(&store, log.session()).unwrap().context;
    assert_eq!(context.turns.len(), 3);
    let rendered = serde_json::to_value(&context.turns).unwrap();
    assert_eq!(rendered[0]["User"]["content"][0]["text"], "before");
    assert!(rendered[1]["User"]["content"][0]["text"].as_str().unwrap().ends_with("summary"));
    assert_eq!(rendered[2]["User"]["content"][0]["text"], "after");
    assert!(!rendered.to_string().contains("middle middle"));
    let invalid = rness_engine::turn::compaction::reduce_region(&store, &mut log, &provider, "", &[],
        &compact_policy(100000), false, Some(2..4), &CancellationToken::new()).await;
    assert!(invalid.is_err());
}

#[tokio::test]
async fn failed_empty_and_nonshrinking_summaries_preserve_region_after_reopen() {
    for (outcome, expected) in [
        (StepOutcome::Failed { error: ProviderError { code: "TEST", message: "failed".into(), retryable: false, retry_after: None }, partial: vec![] }, "failed: TEST: failed"),
        (StepOutcome::Committed(assistant("", StopReason::EndTurn, vec![])), "empty"),
        (StepOutcome::Committed(assistant(&"long".repeat(5000), StopReason::EndTurn, vec![])), "non_shrinking"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut log = store.create(None).unwrap();
        log.append(&SessionEvent::UserMessage(UserMessage { intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text: "original".repeat(1000) }], source: None })).unwrap();
        let session = log.session().clone();
        let original = replay(&store, &session).unwrap().context;
        let provider = Scripted::new(vec![outcome]);
        assert!(!rness_engine::turn::compaction::reduce_region(&store, &mut log, &provider, "", &[],
            &compact_policy(100000), false, Some(0..1), &CancellationToken::new()).await.unwrap());
        drop(log);
        let mut log = store.open(&session).unwrap();
        let size = log.read_all().unwrap().len();
        rness_engine::turn::compaction::recover(&mut log).unwrap();
        assert_eq!(log.read_all().unwrap().len(), size);
        assert_eq!(replay(&store, &session).unwrap().context, original);
        assert!(log.read_all().unwrap().iter().any(|e| matches!(&e.event,
            SessionEvent::CompactionFinished { outcome, .. } if outcome == expected)));
    }
}

#[tokio::test]
async fn compaction_uses_configured_prompts() {
    struct CapturesPrompts;
    #[async_trait]
    impl Provider for CapturesPrompts {
        fn model(&self) -> &str { "test" }
        async fn step(&self, request: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
            assert_eq!(request.system, "Configured system prompt");
            assert!(matches!(&request.context.turns.last().unwrap(), rness_engine::session::projection::ModelTurn::User { content }
                if matches!(&content[0], ContentPart::Text { text } if text == "Configured user prompt")));
            StepOutcome::Committed(assistant("brief", StopReason::EndTurn, vec![]))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    log.append(&SessionEvent::UserMessage(UserMessage { intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: "original ".repeat(1000) }], source: None })).unwrap();
    let mut policy = compact_policy(100000);
    policy.system_prompt = "Configured system prompt".into();
    policy.prompt = "Configured user prompt".into();
    assert!(rness_engine::turn::compaction::reduce_region(&store, &mut log, &CapturesPrompts,
        "", &[], &policy, false, Some(0..1), &CancellationToken::new()).await.unwrap());
}

#[tokio::test]
async fn compaction_omits_unsupported_output_limit() {
    struct RejectsOutputLimit;
    #[async_trait]
    impl Provider for RejectsOutputLimit {
        fn model(&self) -> &str { "subscription" }
        fn supports_max_output_tokens(&self) -> bool { false }
        async fn step(&self, request: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
            assert_eq!(request.context.config.max_output_tokens, None);
            StepOutcome::Committed(assistant("brief", StopReason::EndTurn, vec![]))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    log.append(&SessionEvent::UserMessage(UserMessage { intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: "original ".repeat(1000) }], source: None })).unwrap();
    assert!(rness_engine::turn::compaction::reduce_region(&store, &mut log, &RejectsOutputLimit,
        "", &[], &compact_policy(100000), false, Some(0..1), &CancellationToken::new()).await.unwrap());
}

#[test]
fn interrupted_compaction_recovery_is_idempotent_and_preserves_checkpoint() {
    for committed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut log = store.create(None).unwrap();
        let start = log.append(&SessionEvent::CompactionStarted { model: "test".into(),
            sources: vec![], estimated_input: 10, request: serde_json::json!({"system":"summary", "turns":[]}) }).unwrap();
        if committed {
            log.append(&SessionEvent::Compaction(Compaction { replaces: vec![], summary: "retained".into(), model: "test".into() })).unwrap();
        }
        let session = log.session().clone();
        drop(log);
        let mut log = store.open(&session).unwrap();
        rness_engine::turn::compaction::recover(&mut log).unwrap();
        let recovered_len = log.read_all().unwrap().len();
        drop(log);
        let mut log = store.open(&session).unwrap();
        rness_engine::turn::compaction::recover(&mut log).unwrap();
        assert_eq!(log.read_all().unwrap().len(), recovered_len);
        let history = log.read_all().unwrap();
        let finishes: Vec<_> = history.iter().filter_map(|e| match &e.event {
            SessionEvent::CompactionFinished { started, outcome, .. } => Some((started, outcome.as_str())), _ => None,
        }).collect();
        assert_eq!(finishes, vec![(&start.id, if committed { "committed_before_interruption" } else { "interrupted" })]);
        assert_eq!(history.iter().filter(|e| matches!(e.event, SessionEvent::Compaction(_))).count(), usize::from(committed));
    }
}

#[test]
fn route_meter_changes_estimate_without_changing_context() {
    let context = rness_engine::session::projection::ModelContext {
        turns: vec![rness_engine::session::projection::ModelTurn::User {
            content: vec![ContentPart::Text { text: "test".repeat(200) }],
        }], ..Default::default()
    };
    let original = context.clone();
    let default = rness_engine::turn::compaction::Meter::default();
    let dense = rness_engine::turn::compaction::Meter { bytes_per_token: 2, ..default.clone() };
    assert!(dense.measure(&context, "system", &[]) > default.measure(&context, "system", &[]));
    assert_eq!(context, original);
    let mut invalid = compact_policy(1000);
    invalid.meter.bytes_per_token = 0;
    assert!(invalid.validate().is_err());
    invalid.meter.bytes_per_token = 4;
    invalid.meter.output_reserve = 1000;
    assert!(invalid.validate().is_err());
}

fn compact_policy(threshold: u64) -> rness_engine::turn::compaction::Policy {
    rness_engine::turn::compaction::Policy {
        meter: Default::default(),
        summary_selection: None,
        threshold_tokens: threshold, retain_tokens: 40, summary_tokens: 100,
        system_prompt: "Summarize without tools.".into(),
        prompt: "Create a continuation briefing.".into(),
        max_overflow_retries: 1, max_compactions: 2,
        prune_threshold: 8192, prune_head: 4096, prune_tail: 1024,
    }
}

#[tokio::test]
async fn pre_step_compaction_and_overflow_retry_rederive_from_log() {
    for (overflow, policy_key) in [(false, "test/fake-1"), (true, "test/fake-1"), (false, "default"), (true, "default")] {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut log = store.create(None).unwrap();
        log.append(&SessionEvent::RequestConfig(CallConfig {
            selection: Some(ModelSelection { route: "test".into(), model: "fake-1".into() }),
            ..Default::default()
        })).unwrap();
        for text in ["old ".repeat(3000), "recent".into()] {
            log.append(&SessionEvent::UserMessage(UserMessage { intent: UserIntent::Followup,
                content: vec![ContentPart::Text { text }], source: None })).unwrap();
        }
        let mut steps = vec![];
        if overflow { steps.push(StepOutcome::Failed { error: ProviderError {
            code: "CONTEXT_OVERFLOW", retryable: false, retry_after: None, message: "too long".into(),
        }, partial: vec![] }); }
        steps.push(StepOutcome::Committed(assistant("brief", StopReason::EndTurn, vec![])));
        steps.push(StepOutcome::Committed(assistant("done", StopReason::EndTurn, vec![])));
        let provider = Scripted::new(steps);
        let config = TurnConfig { compaction: [(policy_key.into(), compact_policy(if overflow { 100000 } else { 1000 }))].into(), ..Default::default() };
        // Keep only the latest message in this fixture.
        let mut config = config;
        config.compaction.get_mut(policy_key).unwrap().retain_tokens = 1;
        let outcome = run_turn(&store, &mut log, &provider, &ToolRegistry::default(), &config,
            &CancellationToken::new(), &mut Vec::new, 1, &|_| {}).await.unwrap();
        assert_eq!(outcome, TurnOutcome::Completed);
        let history = store.history(log.session()).unwrap();
        assert_eq!(history.iter().filter(|e| matches!(e.event, SessionEvent::Compaction(_))).count(), 1);
        let started = history.iter().find(|e| matches!(e.event, SessionEvent::CompactionStarted { .. })).unwrap();
        assert!(history.iter().any(|e| matches!(&e.event, SessionEvent::CompactionFinished { started: id, outcome, usage, chunks }
            if id == &started.id && outcome == "committed" && usage.input_tokens == 1 && !chunks.is_empty())));
        let context = replay(&store, log.session()).unwrap().context;
        assert!(!context.sources.contains(&started.id));
        let texts = provider.seen_texts.lock().unwrap();
        assert!(texts.last().unwrap().contains("brief"));
        assert!(texts.last().unwrap().contains("recent"));
        assert!(!texts.last().unwrap().contains("old old"));
        assert!(provider.steps.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn overflowing_indivisible_request_does_not_retry() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    log.append(&SessionEvent::RequestConfig(CallConfig { selection: Some(ModelSelection {
        route: "test".into(), model: "fake-1".into(),
    }), ..Default::default() })).unwrap();
    log.append(&SessionEvent::UserMessage(UserMessage { intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: "huge".repeat(10000) }], source: None })).unwrap();
    let provider = Scripted::new(vec![StepOutcome::Failed { error: ProviderError {
        code: "CONTEXT_OVERFLOW", retryable: false, retry_after: None, message: "too long".into(),
    }, partial: vec![] }]);
    let config = TurnConfig { compaction: [("test/fake-1".into(), compact_policy(1000))].into(), ..Default::default() };
    assert!(run_turn(&store, &mut log, &provider, &ToolRegistry::default(), &config,
        &CancellationToken::new(), &mut Vec::new, 1, &|_| {}).await.is_err());
    assert_eq!(provider.seen_contexts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn boundary_pruning_remeasures_without_summarizing_or_splitting_tools() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    log.append(&SessionEvent::TurnStarted { turn: 1 }).unwrap();
    log.append(&SessionEvent::UserMessage(UserMessage { intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: "work".into() }], source: None })).unwrap();
    log.append(&SessionEvent::AssistantMessage(assistant("", StopReason::ToolUse, vec![("c", "Echo")]))).unwrap();
    let output = "x".repeat(16000);
    log.append(&SessionEvent::ToolResult(ToolResult { call: "c".into(), name: "Echo".into(),
        output: output.clone(), content: vec![ToolResultContentPart::Text { text: output }],
        is_error: false, duration_ms: 0, tasks: None, plan_review: None, presentation: None,
    })).unwrap();
    log.append(&SessionEvent::UserMessage(UserMessage { intent: UserIntent::Steer,
        content: vec![ContentPart::Text { text: "continue".into() }], source: None })).unwrap();
    let provider = Scripted::new(vec![]); // Pruning alone must avoid any model call.
    let mut policy = compact_policy(165000);
    assert!(rness_engine::turn::compaction::measure(&replay(&store, log.session()).unwrap().context, "", &[]) < policy.threshold_tokens);
    policy.retain_tokens = 1;
    assert!(rness_engine::turn::compaction::reduce(&store, &mut log, &provider, "", &[], &policy,
        false, &CancellationToken::new()).await.unwrap());
    let replayed = replay(&store, log.session()).unwrap();
    assert!(rness_engine::turn::compaction::measure(&replayed.context, "", &[]) < 2000);
    assert_eq!(replayed.history.iter().filter(|e| matches!(e.event, SessionEvent::Prune(_))).count(), 1);
    assert!(!replayed.history.iter().any(|e| matches!(e.event, SessionEvent::Compaction(_))));
    // No TurnEnded was required: the existing writer reduced a closed step mid-turn.
    assert!(!replayed.history.iter().any(|e| matches!(e.event, SessionEvent::TurnEnded { .. })));
}

#[tokio::test]
async fn cancelled_summary_does_not_write_a_checkpoint() {
    struct Cancels;
    #[async_trait]
    impl Provider for Cancels {
        fn model(&self) -> &str { "test" }
        async fn step(&self, _: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
            cancel.cancel();
            StepOutcome::Committed(assistant("short", StopReason::EndTurn, vec![]))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    for text in ["old".repeat(3000), "keep".into()] {
        log.append(&SessionEvent::UserMessage(UserMessage { intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text }], source: None })).unwrap();
    }
    let mut policy = compact_policy(1000);
    policy.retain_tokens = 1;
    let cancel = CancellationToken::new();
    assert!(!rness_engine::turn::compaction::reduce(&store, &mut log, &Cancels, "", &[], &policy, false, &cancel).await.unwrap());
    assert!(cancel.is_cancelled());
    assert!(!store.history(log.session()).unwrap().iter().any(|e| matches!(e.event, SessionEvent::Compaction(_))));
}

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

struct RestrictedRequest(AtomicUsize);

#[async_trait]
impl Provider for RestrictedRequest {
    fn model(&self) -> &str { "restricted-test" }
    async fn step(&self, request: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
        assert!(request.system.ends_with("Only review"));
        assert!(request.tools.is_empty());
        if self.0.fetch_add(1, Ordering::SeqCst) > 0 {
            return StepOutcome::Committed(assistant("done", StopReason::EndTurn, vec![]));
        }
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
        run_turn(&store, &mut log, &RestrictedRequest(AtomicUsize::new(0)), &tools,
            &TurnConfig::default(),
            &CancellationToken::new(), &mut no_steers(), 1, &|_| {}).await.unwrap();
        let history = store.history(log.session()).unwrap();
        assert!(history.iter().any(|event| matches!(&event.event, SessionEvent::ToolResult(result) if result.is_error)));
    }
}

#[tokio::test]
async fn tasks_commit_in_model_order_and_survive_compaction_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    let tasks = |content: &str| serde_json::json!({"tasks":[{"id":"one","content":content,"status":"in_progress"}]});
    let mut message = assistant("tracking", StopReason::ToolUse, vec![]);
    for (call, content) in [("a", "first"), ("b", "second")] {
        message.content.push(ContentPart::ToolUse { call: call.into(), name: "TaskWrite".into(), args: tasks(content) });
    }
    let provider = Scripted::new(vec![StepOutcome::Committed(message), StepOutcome::Committed(assistant("done", StopReason::EndTurn, vec![]))]);
    let tools = ToolRegistry::default();
    tools.register(Arc::new(rness_engine::tasks::TaskWrite(Default::default())));
    run_turn(&store, &mut log, &provider, &tools, &TurnConfig::default(), &CancellationToken::new(), &mut no_steers(), 1, &|_| {}).await.unwrap();
    let history = store.history(&sid).unwrap();
    let results: Vec<_> = history.iter().filter_map(|env| match &env.event { SessionEvent::ToolResult(result) => Some(result), _ => None }).collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].tasks.as_ref().unwrap().tasks[0].content, "first");
    assert_eq!(TaskSnapshot::from_history(&history).tasks[0].content, "second");
    log.append(&SessionEvent::Compaction(Compaction { replaces: history.iter().skip(1).map(|env| env.id.clone()).collect(), summary: "compacted".into(), model: "test".into() })).unwrap();
    let fork_at = history.iter().find(|env| matches!(&env.event, SessionEvent::ToolResult(result) if result.call == "a")).unwrap().id.clone();
    drop(log);
    let reopened = SessionStore::new(dir.path());
    let child = reopened.fork(&sid, Some(fork_at)).unwrap();
    assert_eq!(TaskSnapshot::from_history(&reopened.history(child.session()).unwrap()).tasks[0].content, "first");
    assert_eq!(TaskSnapshot::from_history(&replay(&reopened, &sid).unwrap().history).tasks[0].content, "second");
    let other = reopened.create(None).unwrap();
    assert!(TaskSnapshot::from_history(&reopened.history(other.session()).unwrap()).tasks.is_empty());
}

#[tokio::test]
async fn final_tool_result_notifies_after_persistence_without_another_model_step() {
    use rness_protocol::frames::Frame;
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let provider = Scripted::new(vec![StepOutcome::Committed(assistant("calling", StopReason::ToolUse, vec![("last", "Echo")]))]);
    let tools = ToolRegistry::default();
    tools.register(Arc::new(Echo));
    let observed = Mutex::new(false);
    let cancel = CancellationToken::new();
    let frames = |frame| {
        if let Frame::HistoryChanged { session } = frame {
            let history = replay(&store, &session).unwrap().history;
            assert!(history.iter().any(|event| matches!(&event.event, SessionEvent::ToolResult(result) if result.call == "last")));
            *observed.lock().unwrap() = true;
            cancel.cancel();
        }
    };
    let outcome = run_turn(&store, &mut log, &provider, &tools,
        &TurnConfig::default(),
        &cancel, &mut no_steers(), 1, &frames).await.unwrap();
    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert!(*observed.lock().unwrap(), "final tool result did not trigger history/card refresh");
}

#[tokio::test]
async fn tool_roundtrips_continue_beyond_fifty_model_requests() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let mut steps: Vec<_> = (0..60).map(|index| {
        StepOutcome::Committed(assistant("calling", StopReason::ToolUse, vec![(&format!("call-{index}"), "Echo")]))
    }).collect();
    steps.push(StepOutcome::Committed(assistant("done", StopReason::EndTurn, vec![])));
    let provider = Scripted::new(steps);
    let tools = ToolRegistry::default();
    tools.register(Arc::new(Echo));
    let outcome = run_turn(&store, &mut log, &provider, &tools, &TurnConfig::default(),
        &CancellationToken::new(), &mut no_steers(), 1, &|_| {}).await.unwrap();
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(provider.seen_contexts.lock().unwrap().len(), 61);
    assert!(provider.steps.lock().unwrap().is_empty());
    let history = store.history(log.session()).unwrap();
    assert_eq!(history.iter().filter(|event| matches!(event.event, SessionEvent::ToolResult(_))).count(), 60);
    assert!(matches!(&history[history.len() - 2].event,
        SessionEvent::AssistantMessage(message) if message.stop == StopReason::EndTurn));
    assert!(matches!(history.last().unwrap().event,
        SessionEvent::TurnEnded { outcome: TurnOutcome::Completed, .. }));
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
            SessionEvent::ToolsActivated { .. } => "tools-activated",
            SessionEvent::ProgramToolStarted { .. } => "program-started",
            SessionEvent::ProgramToolResult { .. } => "program-result",
            SessionEvent::Header(_) => "header",
            SessionEvent::UserMessage(_) => "user",
            SessionEvent::TurnStarted { .. } => "turn+",
            SessionEvent::AssistantMessage(_) => "assistant",
            SessionEvent::ToolResult(_) => "tool",
            SessionEvent::TurnEnded { .. } => "turn-",
            SessionEvent::AssistantAttempt(_) => "attempt",
            SessionEvent::CompactionRequest { .. } => "compaction-request",
            SessionEvent::CompactionStarted { .. } => "compaction+",
            SessionEvent::CompactionFinished { .. } => "compaction-",
            SessionEvent::Compaction(_) => "compaction",
            SessionEvent::Prune(_) => "prune",
            SessionEvent::PlanMode { .. } => "plan",
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
                source: None,
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
            error: ProviderError { code: "PROVIDER", retry_after: None, message: "overloaded".into(), retryable: true },
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
        error: ProviderError { code: "PROVIDER", retry_after: None, message: "invalid api key".into(), retryable: false },
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
