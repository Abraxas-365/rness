//! The delegation tool through the REAL dispatch path: the model calls
//! `subagent`, the dispatcher passes the calling session, the child runs
//! and its answer comes back as an ordinary tool result.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::subagent::{ForkProvider, SpawnProvider, SubagentRuntime};
use rness_engine::tools::{ToolCall, ToolRegistry};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::EventBus;
use rness_protocol::events::*;
use tokio_util::sync::CancellationToken;

struct OneAnswer;

#[async_trait]
impl Provider for OneAnswer {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, _request: StepRequest<'_>, _cancel: &CancellationToken) -> StepOutcome {
        StepOutcome::Committed(AssistantMessage {
            model: "fake-1".into(),
            content: vec![ContentPart::Text { text: "hecho".into() }],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
            chunks: vec![],
        })
    }
}

fn compose(dir: &std::path::Path) -> (Arc<SessionService>, Arc<ToolRegistry>) {
    let tools = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir),
        Arc::new(OneAnswer),
        Arc::clone(&tools),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ).with_agents([("worker".into(), rness_engine::config::AgentDefinition {
        subagent: true, description: "Implementation".into(), instructions: "Implement carefully".into(),
        profile: None, tools: None,
    })].into(), Default::default()));
    let runtime = Arc::new(SubagentRuntime::new(Arc::clone(&sessions), 3));
    runtime.register(Arc::new(SpawnProvider));
    runtime.register(Arc::new(ForkProvider));
    let jobs = rness_tools::jobs::JobRegistry::new();
    tools.register(Arc::new(rness_tools::jobs::JobOutputTool::new(jobs.clone())));
    rness_tools::register_subagent(&tools, Arc::clone(&runtime), jobs);
    rness_tools::subagent_control::register_subagent_control(&tools, runtime);
    (sessions, tools)
}

#[tokio::test(flavor = "multi_thread")]
async fn subagent_tool_delegates_through_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let (sessions, tools) = compose(dir.path());
    let parent = sessions.create(None).unwrap();

    let calls = vec![ToolCall {
        call: "c1".into(),
        name: "subagent".into(),
        args: serde_json::json!({ "provider": "spawn", "agent": "worker", "prompt": "delega esto" }),
    }];
    let results = tools.dispatch(&parent, &calls, 1, &Default::default()).await;

    assert!(!results[0].is_error, "{}", results[0].output);
    assert!(results[0].output.contains("hecho"));
    assert!(results[0].output.contains("[subagent session:"));
    // The child exists with lineage stamped.
    assert_eq!(sessions.list().unwrap().len(), 2);
    let child = sessions.list().unwrap().into_iter().find(|id| id != &parent).unwrap();
    assert_eq!(sessions.config(&child).unwrap().agent.unwrap().name, "worker");
    let schema = tools.get("subagent").unwrap().input_schema();
    assert_eq!(schema["properties"]["agent"]["enum"], serde_json::json!(["worker"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn background_subagent_is_a_job() {
    let dir = tempfile::tempdir().unwrap();
    let (sessions, tools) = compose(dir.path());
    let parent = sessions.create(None).unwrap();

    let calls = vec![ToolCall {
        call: "c1".into(),
        name: "subagent".into(),
        args: serde_json::json!({
            "provider": "spawn", "prompt": "en background", "run_in_background": true
        }),
    }];
    let results = tools.dispatch(&parent, &calls, 1, &Default::default()).await;
    assert!(!results[0].is_error);
    assert!(results[0].output.contains("job"), "{}", results[0].output);

    // The job settles with the child's output readable via job_output.
    let id = results[0]
        .output
        .split_whitespace()
        .find(|w| w.starts_with('j') && w[1..].chars().all(|c| c.is_ascii_digit()))
        .expect("job id in output")
        .to_string();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let out = tools
            .dispatch(
                &parent,
                &[ToolCall {
                    call: "c2".into(),
                    name: "job_output".into(),
                    args: serde_json::json!({ "job_id": id }),
                }],
                1,
            &Default::default(),
            )
            .await;
        if out[0].output.contains("hecho") {
            return;
        }
    }
    panic!("background subagent never settled");
}

#[tokio::test(flavor = "multi_thread")]
async fn continuable_lifecycle_through_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let (sessions, tools) = compose(dir.path());
    let parent = sessions.create(None).unwrap();

    // Start a continuable child through the model-facing tool.
    let results = tools
        .dispatch(
            &parent,
            &[ToolCall {
                call: "c1".into(),
                name: "subagent".into(),
                args: serde_json::json!({
                    "provider": "spawn", "prompt": "quedate",
                    "background_mode": "continuable"
                }),
            }],
            1,
            &Default::default(),
        )
        .await;
    assert!(!results[0].is_error, "{}", results[0].output);
    let child = results[0]
        .output
        .split_whitespace()
        .find(|w| w.len() == 26 && w.chars().all(|c| c.is_ascii_alphanumeric()))
        .expect("child id in output")
        .to_string();
    sessions.join(&child).await;

    // list_agents sees it.
    let listed = tools
        .dispatch(
            &parent,
            &[ToolCall {
                call: "c2".into(),
                name: "list_agents".into(),
                args: serde_json::json!({}),
            }],
            1,
            &Default::default(),
        )
        .await;
    assert!(listed[0].output.contains(&child), "{}", listed[0].output);

    // send_message reaches it; interrupt_agent is an accepted no-op.
    let sent = tools
        .dispatch(
            &parent,
            &[
                ToolCall {
                    call: "c3".into(),
                    name: "send_message".into(),
                    args: serde_json::json!({ "agent_id": child, "message": "sigue" }),
                },
                ToolCall {
                    call: "c4".into(),
                    name: "interrupt_agent".into(),
                    args: serde_json::json!({ "agent_id": child }),
                },
            ],
            1,
            &Default::default(),
        )
        .await;
    assert!(!sent[0].is_error, "{}", sent[0].output);
    assert!(sent[0].output.contains("accepted"));
    assert!(!sent[1].is_error, "{}", sent[1].output);
    sessions.join(&child).await;

    // A stranger session gets a loud refusal.
    let stranger = sessions.create(None).unwrap();
    let refused = tools
        .dispatch(
            &stranger,
            &[ToolCall {
                call: "c5".into(),
                name: "send_message".into(),
                args: serde_json::json!({ "agent_id": child, "message": "hola" }),
            }],
            1,
            &Default::default(),
        )
        .await;
    assert!(refused[0].is_error);
    assert!(refused[0].output.contains("not delivered"), "{}", refused[0].output);
}
