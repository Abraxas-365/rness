//! Structured output on the subagent seam (dsh `structured.ts` parity):
//! child-scoped capture tool + instruction, retry on invalid args, turn
//! conclusion on capture, terminal guard, error on missing capture, and
//! in-memory-only scoping (no leak to parent/siblings, no residue).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::session::projection::ModelTurn;
use rness_engine::subagent::{
    RunOptions, SpawnProvider, StopReason, SubagentError, SubagentRequest, SubagentRuntime,
};
use rness_engine::tools::{Tool, ToolRegistry};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::EventBus;
use rness_protocol::events::StopReason as Stop;
use rness_protocol::events::*;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// What each step of a session saw: (system prompt, advertised tool names).
type Seen = Arc<Mutex<HashMap<String, Vec<(String, Vec<String>)>>>>;

/// Scripted by the first user message of the session; step index = number
/// of assistant turns already in context.
struct Scripted {
    seen: Seen,
}

fn first_user_text(request: &StepRequest<'_>) -> String {
    request
        .context
        .turns
        .iter()
        .find_map(|t| match t {
            ModelTurn::User { content } => content.iter().find_map(|p| match p {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .unwrap_or_default()
}

fn message(text: &str, calls: Vec<(&str, &str, Value)>) -> AssistantMessage {
    let mut content = vec![ContentPart::Text { text: text.into() }];
    let stop = if calls.is_empty() {
        Stop::EndTurn
    } else {
        Stop::ToolUse
    };
    for (call, name, args) in calls {
        content.push(ContentPart::ToolUse {
            call: call.into(),
            name: name.into(),
            args,
        });
    }
    AssistantMessage {
        model: "fake-1".into(),
        content,
        stop,
        usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        estimated_input: 0,
        chunks: vec![],
    }
}

#[async_trait]
impl Provider for Scripted {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        let prompt = first_user_text(&request);
        let step = request
            .context
            .turns
            .iter()
            .filter(|t| matches!(t, ModelTurn::Assistant { .. }))
            .count();
        self.seen
            .lock()
            .unwrap()
            .entry(prompt.clone())
            .or_default()
            .push((
                request.system.to_string(),
                request.tools.iter().map(|t| t.name.clone()).collect(),
            ));
        let valid = json!({"verdict":"ok","count":2});
        StepOutcome::Committed(match (prompt.as_str(), step) {
            // Valid capture followed by another call in the same response.
            ("valid", 0) => message(
                "",
                vec![
                    ("c1", "structured_output", valid),
                    ("c2", "echo", json!({})),
                ],
            ),
            ("retry", 0) => message(
                "",
                vec![(
                    "c1",
                    "structured_output",
                    json!({"verdict":"ok","count":"two"}),
                )],
            ),
            ("retry", 1) => message("", vec![("c2", "structured_output", valid)]),
            ("hang", _) => std::future::pending().await,
            ("wait", _) => {
                cancel.cancelled().await;
                return StepOutcome::Cancelled { partial: vec![] };
            }
            // "plain", parents, and anything after a step we did not script.
            _ => message("just text", vec![]),
        })
    }
}

/// Counts executions so the guard can be observed.
struct Echo(Arc<Mutex<u32>>);

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    async fn execute(&self, _args: Value) -> Result<String, String> {
        *self.0.lock().unwrap() += 1;
        Ok("echoed".into())
    }
}

struct Harness {
    sessions: Arc<SessionService>,
    rt: SubagentRuntime,
    seen: Seen,
    echoes: Arc<Mutex<u32>>,
    parent: SessionId,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let seen: Seen = Arc::default();
    let echoes = Arc::new(Mutex::new(0));
    let tools = ToolRegistry::default();
    tools.register(Arc::new(Echo(Arc::clone(&echoes))));
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(Scripted {
            seen: Arc::clone(&seen),
        }),
        Arc::new(tools),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let rt = SubagentRuntime::new(Arc::clone(&sessions), 3).with_allow_generic(true);
    rt.register(Arc::new(SpawnProvider));
    let parent = sessions.create(None).unwrap();
    sessions
        .send(
            &parent,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "parent".into(),
            }],
        )
        .unwrap();
    sessions.join(&parent).await;
    Harness {
        sessions,
        rt,
        seen,
        echoes,
        parent,
        _dir: dir,
    }
}

fn schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "verdict":{"type":"string","enum":["ok","bad"]},
            "count":{"type":"integer"}
        },
        "required":["verdict","count"],
        "additionalProperties":false
    })
}

fn request(h: &Harness, prompt: &str) -> SubagentRequest {
    SubagentRequest {
        agent: None,
        parent: h.parent.clone(),
        prompt: prompt.into(),
    }
}

fn structured(schema: Value) -> RunOptions {
    RunOptions {
        output_schema: Some(schema),
        ..RunOptions::default()
    }
}

fn tool_results(h: &Harness, child: &SessionId) -> Vec<ToolResult> {
    h.sessions
        .store()
        .history(child)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn valid_capture_concludes_turn_and_guards_later_calls() {
    let h = harness().await;
    let run =
        h.rt.start_with("spawn", request(&h, "valid"), structured(schema()))
            .await
            .unwrap();
    assert_eq!(run.stop, StopReason::Completed);
    assert_eq!(run.structured, Some(json!({"verdict":"ok","count":2})));
    // Concluded: exactly one model step for the child.
    assert_eq!(h.seen.lock().unwrap()["valid"].len(), 1);
    // Guard: the call after the capture committed a refusal and never ran.
    let results = tool_results(&h, &run.session);
    assert_eq!(results.len(), 2);
    assert!(!results[0].is_error);
    assert_eq!(results[0].output, "Structured output recorded.");
    assert!(results[1].is_error);
    assert!(results[1]
        .output
        .contains("structured output already recorded"));
    assert_eq!(*h.echoes.lock().unwrap(), 0);
    // No residue once settled.
    assert!(!h.sessions.structured_outputs().is_attached(&run.session));
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_args_retry_in_the_same_turn() {
    let h = harness().await;
    let run =
        h.rt.start_with("spawn", request(&h, "retry"), structured(schema()))
            .await
            .unwrap();
    assert_eq!(run.stop, StopReason::Completed);
    assert_eq!(run.structured, Some(json!({"verdict":"ok","count":2})));
    let results = tool_results(&h, &run.session);
    assert!(results[0].is_error);
    assert!(results[0].output.contains("\"count\" must be an integer"));
    assert!(!results[1].is_error);
    assert_eq!(h.seen.lock().unwrap()["retry"].len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_text_finish_is_an_error() {
    let h = harness().await;
    let run =
        h.rt.start_with("spawn", request(&h, "plain"), structured(schema()))
            .await
            .unwrap();
    assert_eq!(run.stop, StopReason::Error);
    assert_eq!(run.structured, None);
    assert_eq!(run.output, "just text");
}

#[tokio::test(flavor = "multi_thread")]
async fn capture_tool_and_instruction_are_child_scoped() {
    let h = harness().await;
    h.rt.start_with("spawn", request(&h, "valid"), structured(schema()))
        .await
        .unwrap();
    // A sibling without a schema, and a plain start, see nothing extra.
    let plain = h.rt.start("spawn", request(&h, "sibling")).await.unwrap();
    assert_eq!(plain.stop, StopReason::Completed);
    assert_eq!(plain.structured, None);
    let seen = h.seen.lock().unwrap();
    let (system, tools) = &seen["valid"][0];
    assert!(system.contains("MUST report it by calling the `structured_output` tool"));
    assert!(tools.contains(&"structured_output".to_string()));
    for other in ["parent", "sibling"] {
        for (system, tools) in &seen[other] {
            assert!(!system.contains("structured_output"), "{other}");
            assert!(!tools.contains(&"structured_output".to_string()), "{other}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn role_tool_restrictions_cannot_strip_the_capture_tool() {
    let h = harness().await;
    // Parent's ceiling is empty → the child inherits no tools at all.
    let mut config = h.sessions.config(&h.parent).unwrap();
    config.tool_ceiling = Some(vec![]);
    h.sessions.set_config(&h.parent, config).unwrap();
    let run =
        h.rt.start_with("spawn", request(&h, "valid"), structured(schema()))
            .await
            .unwrap();
    assert_eq!(run.structured, Some(json!({"verdict":"ok","count":2})));
    let seen = h.seen.lock().unwrap();
    assert_eq!(seen["valid"][0].1, vec!["structured_output".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_schema_fails_before_any_child_exists() {
    let h = harness().await;
    let before = h.sessions.list().unwrap();
    let err =
        h.rt.start_with(
            "spawn",
            request(&h, "valid"),
            structured(json!({"type":"object","properties":{"a":{"type":"string","pattern":"x"}}})),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SubagentError::InvalidSchema(_)));
    assert!(err
        .to_string()
        .contains("pattern is not a supported keyword"));
    assert_eq!(h.sessions.list().unwrap(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_run_detaches() {
    let h = harness().await;
    let before = h.sessions.list().unwrap().len();
    let started = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        h.rt.start_with("spawn", request(&h, "hang"), structured(schema())),
    )
    .await;
    assert!(started.is_err(), "hang child must not settle");
    let child = h
        .sessions
        .list()
        .unwrap()
        .into_iter()
        .find(|id| id != &h.parent)
        .expect("child created");
    assert_eq!(h.sessions.list().unwrap().len(), before + 1);
    assert!(!h.sessions.structured_outputs().is_attached(&child));
    h.sessions.cancel(&child);
}

#[tokio::test(flavor = "multi_thread")]
async fn run_options_cancel_aborts_and_on_start_reports_the_child() {
    let h = harness().await;
    let cancel = CancellationToken::new();
    let seen = Arc::new(Mutex::new(None));
    let (trigger, sink) = (cancel.clone(), seen.clone());
    let run = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.rt.start_with(
            "spawn",
            request(&h, "wait"),
            RunOptions {
                cancel: Some(cancel),
                on_start: Some(Box::new(move |child: &SessionId| {
                    *sink.lock().unwrap() = Some(child.clone());
                    // Cancel before the prompt is even sent: the watcher
                    // must still end the turn.
                    trigger.cancel();
                })),
                ..structured(schema())
            },
        ),
    )
    .await
    .expect("cancelled child must settle")
    .unwrap();
    assert_eq!(run.stop, StopReason::Aborted);
    assert_eq!(seen.lock().unwrap().as_deref(), Some(run.session.as_str()));
    assert!(!h.sessions.structured_outputs().is_attached(&run.session));
}
