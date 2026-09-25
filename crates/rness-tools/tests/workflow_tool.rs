//! The `workflow` tool through a REAL parent turn: the model calls
//! `workflow`, members are real subagent sessions of the caller, and the
//! run's value (or failure) comes back as an ordinary tool result carrying
//! the card's presentation metadata.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::session::projection::ModelTurn;
use rness_engine::subagent::{SpawnProvider, SubagentRuntime};
use rness_engine::tools::{ToolCall, ToolRegistry};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_engine::workflow::WorkflowActivity;
use rness_kernel::EventBus;
use rness_protocol::events::StopReason as Stop;
use rness_protocol::events::*;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Advertised tool names per session prompt, one entry per step.
type Seen = Arc<Mutex<HashMap<String, Vec<Vec<String>>>>>;

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

fn message(text: &str, call: Option<(&str, Value)>) -> AssistantMessage {
    let mut content = vec![ContentPart::Text { text: text.into() }];
    let stop = if call.is_some() {
        Stop::ToolUse
    } else {
        Stop::EndTurn
    };
    if let Some((name, args)) = call {
        content.push(ContentPart::ToolUse {
            call: format!("call-{name}"),
            name: name.into(),
            args,
        });
    }
    AssistantMessage {
        model: "fake-1".into(),
        content,
        stop,
        usage: Usage::default(),
        estimated_input: 0,
        chunks: vec![],
    }
}

fn workflow_call(script: &str, args: Value) -> Value {
    json!({
        "meta": {"name":"test-run","description":"integration","phases":[{"title":"Scan"}]},
        "script": script,
        "args": args,
    })
}

const OK_SCRIPT: &str = r#"
phase("Scan")
log("starting")
local shape = { type = "object", required = { "n" }, properties = { n = { type = "integer" } } }
local rows = pipeline(args.items,
  function(_, item) return agent("structured " .. item, { label = "scan " .. item, schema = shape }) end,
  function(prev, item) return prev.n * 10 end)
local text = agent("text hello", { label = "greet" })
local failed = agent("fail now", { label = "doomed" })
return { rows = rows, text = text, failed = failed == nil }
"#;

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
            .push(request.tools.iter().map(|t| t.name.clone()).collect());
        if step > 0 {
            return StepOutcome::Committed(message("done", None));
        }
        let reply = if prompt == "parent ok" {
            message(
                "",
                Some((
                    "workflow",
                    workflow_call(OK_SCRIPT, json!({"items":["1","2","3"]})),
                )),
            )
        } else if prompt == "parent misuse" {
            message(
                "",
                Some((
                    "workflow",
                    workflow_call("return agent('text x', { effort = 'high' })", json!({})),
                )),
            )
        } else if prompt == "parent cancel" {
            message(
                "",
                Some((
                    "workflow",
                    workflow_call(
                        "return parallel({ function() return agent('wait a') end, function() return agent('wait b') end })",
                        json!({}),
                    ),
                )),
            )
        } else if let Some(n) = prompt.strip_prefix("structured ") {
            message(
                "",
                Some(("structured_output", json!({"n": n.parse::<i64>().unwrap()}))),
            )
        } else if prompt == "text hello" {
            message("hello back", None)
        } else if prompt == "fail now" {
            return StepOutcome::Failed {
                error: rness_engine::turn::provider::ProviderError {
                    code: "PROVIDER",
                    retry_after: None,
                    message: "boom".into(),
                    retryable: false,
                },
                partial: vec![],
            };
        } else if prompt.starts_with("wait") {
            cancel.cancelled().await;
            return StepOutcome::Cancelled { partial: vec![] };
        } else {
            message("unscripted", None)
        };
        StepOutcome::Committed(reply)
    }
}

struct Harness {
    sessions: Arc<SessionService>,
    tools: Arc<ToolRegistry>,
    activity: Arc<WorkflowActivity>,
    seen: Seen,
    _dir: tempfile::TempDir,
}

fn harness(allow_generic: bool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let seen: Seen = Arc::default();
    let tools = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(Scripted {
            seen: Arc::clone(&seen),
        }),
        Arc::clone(&tools),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let runtime =
        Arc::new(SubagentRuntime::new(Arc::clone(&sessions), 3).with_allow_generic(allow_generic));
    runtime.register(Arc::new(SpawnProvider));
    let jobs = rness_tools::jobs::JobRegistry::new();
    rness_tools::register_subagent(&tools, Arc::clone(&runtime), jobs);
    let activity = Arc::new(WorkflowActivity::default());
    rness_tools::register_workflow(
        &tools,
        runtime,
        Arc::clone(&activity),
        rness_tools::workflow::WorkflowConfig::default(),
    );
    Harness {
        sessions,
        tools,
        activity,
        seen,
        _dir: dir,
    }
}

fn workflow_result(h: &Harness, session: &SessionId) -> ToolResult {
    h.sessions
        .store()
        .history(session)
        .unwrap()
        .into_iter()
        .find_map(|e| match e.event {
            SessionEvent::ToolResult(r) if r.name == "workflow" => Some(r),
            _ => None,
        })
        .expect("workflow tool result")
}

async fn run_parent(h: &Harness, prompt: &str) -> SessionId {
    let parent = h.sessions.create(None).unwrap();
    h.sessions
        .send(
            &parent,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: prompt.into(),
            }],
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), h.sessions.join(&parent))
        .await
        .expect("parent turn settles");
    parent
}

#[tokio::test(flavor = "multi_thread")]
async fn workflow_runs_members_as_subagents_and_returns_the_value() {
    let h = harness(true);
    let parent = run_parent(&h, "parent ok").await;
    let result = workflow_result(&h, &parent);
    assert!(!result.is_error, "{}", result.output);
    assert!(
        result
            .output
            .starts_with("workflow \"test-run\" completed (5 agents)."),
        "{}",
        result.output
    );
    let body = result.output.split_once('\n').unwrap().1;
    let value: Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        value,
        json!({"rows":[10,20,30],"text":"hello back","failed":true})
    );

    // Durable card metadata: the frozen final snapshot.
    let meta = result.presentation.expect("presentation");
    assert_eq!(meta["kind"], "workflow_activity");
    assert_eq!(meta["status"], "completed");
    assert_eq!(meta["name"], "test-run");
    assert_eq!(meta["phase"], "Scan");
    assert_eq!(meta["total"], 5);
    assert_eq!(meta["counts"]["completed"], 4);
    assert_eq!(meta["counts"]["failed"], 1);
    assert_eq!(meta["logs"], json!(["starting"]));
    let labels: Vec<_> = meta["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["label"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(labels, ["scan 1", "scan 2", "scan 3", "greet", "doomed"]);

    // Members are ordinary one-shot children of the caller, attributed to
    // the workflow call, and cannot start workflows themselves.
    let children: Vec<_> = h
        .sessions
        .store()
        .delegated_children(&parent)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(children.len(), 5);
    for (child, link) in &children {
        assert_eq!(link.call.as_deref(), Some("call-workflow"));
        let ceiling = h.sessions.config(child).unwrap().tool_ceiling.unwrap();
        assert!(!ceiling.contains(&"workflow".to_string()), "{ceiling:?}");
        assert!(ceiling.contains(&"subagent".to_string()));
    }
    let seen = h.seen.lock().unwrap();
    assert!(seen["parent ok"][0].contains(&"workflow".to_string()));
    for prompt in ["structured 1", "text hello", "fail now"] {
        for tools in &seen[prompt] {
            assert!(
                !tools.contains(&"workflow".to_string()),
                "{prompt}: {tools:?}"
            );
        }
    }
    // Live registry holds the same final state for the card.
    let snaps = h.activity.card_snapshots(&parent);
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].0, "call-workflow");
    assert_eq!(snaps[0].2["status"], "completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn misuse_is_an_error_result_with_card_state() {
    let h = harness(true);
    let parent = run_parent(&h, "parent misuse").await;
    let result = workflow_result(&h, &parent);
    assert!(result.is_error);
    assert!(
        result
            .output
            .contains("workflow \"test-run\" failed after starting 0 agents")
            && result.output.contains("\"effort\" is deferred"),
        "{}",
        result.output
    );
    let meta = result.presentation.expect("presentation");
    assert_eq!(meta["status"], "error");
    assert!(meta["error"].as_str().unwrap().contains("effort"));
    assert!(h
        .sessions
        .store()
        .delegated_children(&parent)
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_the_parent_cancels_members_and_the_run() {
    let h = harness(true);
    let parent = h.sessions.create(None).unwrap();
    h.sessions
        .send(
            &parent,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "parent cancel".into(),
            }],
        )
        .unwrap();
    // Wait until both members are running.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let snaps = h.activity.card_snapshots(&parent);
        if snaps
            .first()
            .is_some_and(|(_, _, s)| s["counts"]["running"] == 2)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "members never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    h.sessions.cancel(&parent);
    tokio::time::timeout(std::time::Duration::from_secs(20), h.sessions.join(&parent))
        .await
        .expect("parent settles after cancel");
    let snaps = h.activity.card_snapshots(&parent);
    assert_eq!(snaps[0].2["status"], "cancelled");
    assert_eq!(snaps[0].2["counts"]["running"], 0);
    // Every member's own turn ended (none left running).
    for (child, _) in h.sessions.store().delegated_children(&parent).unwrap() {
        assert_ne!(
            h.sessions.phase(&child),
            rness_engine::inbox::Phase::Running,
            "{child} still running"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn arguments_are_validated_before_any_run() {
    let h = harness(true);
    let parent = h.sessions.create(None).unwrap();
    let dispatch = |args: Value| {
        let (tools, parent) = (Arc::clone(&h.tools), parent.clone());
        async move {
            tools
                .dispatch(
                    &parent,
                    &[ToolCall {
                        call: "c".into(),
                        name: "workflow".into(),
                        args,
                    }],
                    1,
                    &CancellationToken::new(),
                )
                .await
                .remove(0)
        }
    };
    for (args, needle) in [
        (
            json!({"meta":{"name":"Bad Name","description":"d"},"script":"return 1"}),
            "kebab-case",
        ),
        (
            json!({"meta":{"name":"a","description":"d"},"script":"return ("}),
            "does not parse",
        ),
        (
            json!({"meta":{"name":"a","description":"d"},"script":"  "}),
            "non-empty",
        ),
        (
            json!({"meta":{"name":"a","description":"d"},"script":"return 1","args":[1]}),
            "JSON object",
        ),
        (
            json!({"meta":{"name":"a","description":"d"},"script":"return 1","extra":1}),
            "not recognized",
        ),
    ] {
        let result = dispatch(args).await;
        assert!(result.is_error);
        assert!(
            result.output.contains(needle),
            "{needle}: {}",
            result.output
        );
    }
    assert!(h.activity.card_snapshots(&parent).is_empty());
    // JSON-encoded meta/args strings are accepted (some models send them).
    let result = dispatch(json!({
        "meta": "{\"name\":\"a\",\"description\":\"d\"}",
        "script": "return args.x + 1",
        "args": "{\"x\": 41}",
    }))
    .await;
    assert!(!result.is_error, "{}", result.output);
    assert!(result.output.ends_with("\n42"), "{}", result.output);
}

#[tokio::test(flavor = "multi_thread")]
async fn unavailable_without_generic_children_or_roles() {
    let h = harness(false);
    let tool = h.tools.get("workflow").unwrap();
    assert!(tool.description().starts_with("Workflows are unavailable"));
    let parent = h.sessions.create(None).unwrap();
    let result = h
        .tools
        .dispatch(
            &parent,
            &[ToolCall {
                call: "c".into(),
                name: "workflow".into(),
                args: json!({"meta":{"name":"a","description":"d"},"script":"return 1"}),
            }],
            1,
            &CancellationToken::new(),
        )
        .await
        .remove(0);
    assert!(result.is_error);
    assert!(result.output.starts_with("Workflows are unavailable"));
    // With generic children allowed, the full contract is advertised.
    let h = harness(true);
    let description = h.tools.get("workflow").unwrap().description().to_owned();
    for needle in [
        "agent(prompt, opts?)",
        "pipeline(items",
        "parallel(thunks)",
        "compact(list)",
    ] {
        assert!(description.contains(needle), "{needle}");
    }
}
