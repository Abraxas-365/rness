//! Subagent seam end-to-end against a scripted provider: spawn is
//! fresh, fork inherits the completed prefix, depth is enforced from
//! durable stamps, and the child is an ordinary session in the store.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::subagent::{
    ForkProvider, SpawnProvider, StopReason, SubagentError, SubagentRequest, SubagentRuntime,
};
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::EventBus;
use rness_protocol::events::*;
use tokio_util::sync::CancellationToken;

/// Answers with a text that reveals how much history the model saw —
/// the fork/spawn difference becomes observable output.
struct EchoCount;

#[async_trait]
impl Provider for EchoCount {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, request: StepRequest<'_>, _cancel: &CancellationToken) -> StepOutcome {
        let seen = request.context.turns.len();
        StepOutcome::Committed(AssistantMessage {
            model: "fake-1".into(),
            content: vec![ContentPart::Text { text: format!("saw {seen} turns") }],
            stop: StopReason2::EndTurn,
            usage: Usage { input_tokens: 1, output_tokens: 1, ..Default::default() },
            estimated_input: 0,
            chunks: vec![],
        })
    }
}

use rness_protocol::events::StopReason as StopReason2;

fn service(dir: &std::path::Path) -> Arc<SessionService> {
    Arc::new(SessionService::new(
        SessionStore::new(dir),
        Arc::new(EchoCount),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ))
}

fn runtime(sessions: &Arc<SessionService>, max_depth: u32) -> SubagentRuntime {
    let rt = SubagentRuntime::new(Arc::clone(sessions), max_depth).with_allow_generic(true);
    rt.register(Arc::new(SpawnProvider));
    rt.register(Arc::new(ForkProvider));
    rt
}

#[tokio::test]
async fn generic_children_require_explicit_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = SubagentRuntime::new(sessions.clone(), 3);
    rt.register(Arc::new(SpawnProvider));
    rt.register(Arc::new(ForkProvider));
    assert!(!rt.allows_generic());
    let parent = sessions.create(None).unwrap();
    one_completed_turn(&sessions, &parent).await;
    for provider in ["spawn", "fork"] {
        let request = SubagentRequest { agent: None, parent: parent.clone(), prompt: "task".into() };
        let before = sessions.list().unwrap();
        assert!(rt.start(provider, request.clone()).await.unwrap_err().to_string()
            .contains("generic subagents are disabled"));
        assert!(rt.start_continuable(provider, request).is_err());
        assert_eq!(sessions.list().unwrap(), before);
    }
    let rt = rt.with_allow_generic(true);
    for provider in ["spawn", "fork"] {
        let request = SubagentRequest { agent: None, parent: parent.clone(), prompt: "task".into() };
        let run = rt.start(provider, request.clone()).await.unwrap();
        assert!(sessions.config(&run.session).unwrap().agent.is_none());
        let child = rt.start_continuable(provider, request).unwrap();
        sessions.join(&child).await;
    }
}

/// Run one parent turn so there is a completed prefix to inherit.
async fn one_completed_turn(sessions: &Arc<SessionService>, id: &SessionId) {
    sessions
        .send(id, UserIntent::Followup, vec![ContentPart::Text { text: "hola".into() }])
        .unwrap();
    sessions.join(id).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn activity_is_scoped_to_parent_call_and_excludes_fork_seed() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();
    one_completed_turn(&sessions, &parent).await;
    for (mode, call) in [("spawn", "a"), ("fork", "b")] {
        rt.start_presented(mode, SubagentRequest {
            parent: parent.clone(), agent: None, prompt: "inspect".into(),
        }, Some((call.into(), serde_json::json!({"prompt":"inspect"})))).await.unwrap();
    }
    assert!(rt.activity.snapshots(&sessions, "other").is_empty());
    let snapshots = rt.activity.snapshots(&sessions, &parent);
    assert_eq!(snapshots.len(), 2);
    for (call, args, view) in &snapshots {
        assert!(["a", "b"].contains(&call.as_str()));
        assert_eq!(args["prompt"], "inspect");
        assert_eq!(view["status"], "completed");
        assert_eq!(view["lines"].as_array().unwrap().len(), 1);
    }
    let again = rt.activity.snapshots(&sessions, &parent);
    assert_eq!(snapshots, again);
    let mut log = rness_engine::session::log::SessionLog::open(dir.path(), &parent).unwrap();
    log.append(&SessionEvent::AssistantMessage(AssistantMessage {
        model: "fake".into(), content: ["a", "b"].into_iter().map(|call| ContentPart::ToolUse {
            call: call.into(), name: "subagent".into(), args: serde_json::json!({"prompt":"inspect"}),
        }).collect(), stop: StopReason2::ToolUse, usage: Usage::default(), estimated_input: 0, chunks: vec![],
    })).unwrap();
    drop(log);
    let reopened = service(dir.path());
    let recovered = runtime(&reopened, 3);
    recovered.activity.recover(&reopened, &parent).unwrap();
    let restored = recovered.activity.snapshots(&reopened, &parent);
    assert_eq!(restored.len(), 2);
    for (call, _, view) in &restored {
        let old = snapshots.iter().find(|(id, _, _)| id == call).unwrap();
        assert_eq!(view["lines"], old.2["lines"]);
        assert_eq!(view["status"], "completed");
    }
    recovered.activity.recover(&reopened, &parent).unwrap();
    assert_eq!(restored, recovered.activity.snapshots(&reopened, &parent));
    let child = snapshots[0].2["session"].as_str().unwrap().to_string();
    rt.activity.observe(&rness_protocol::frames::Frame::Delta {
        session: child.clone(), chunk: ChunkDelta::Text { t: "live text".into() },
    });
    assert!(rt.activity.snapshots(&sessions, &parent).iter().any(|(_, _, view)| view["live"] == "live text"));
    rt.activity.observe(&rness_protocol::frames::Frame::StepCommitted { session: child, event: "commit".into() });
    assert!(rt.activity.snapshots(&sessions, &parent).iter().all(|(_, _, view)| view["live"] == ""));
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_profiles_follow_parent_without_inheriting_model_options() {
    use rness_engine::config::{AgentDefinition, ModelRegistry, Profile};
    let dir = tempfile::tempdir().unwrap();
    let mut models = ModelRegistry::default();
    let profile: Profile = serde_json::from_value(serde_json::json!({"by_provider": {
        "one": {"model":"cheap-one", "options":{"max_output_tokens":200}},
        "two": {"model":"cheap-two", "options":{"max_output_tokens":300}}
    }})).unwrap();
    models.declare_profile("small".into(), profile).unwrap();
    let agents = [("scout".into(), AgentDefinition { subagent:true, description:"Scout".into(),
        instructions:"Inspect".into(), profile:Some("small".into()), tools:Some(vec![]), sandbox: None })].into();
    let sessions = Arc::new(SessionService::new(SessionStore::new(dir.path()), Arc::new(EchoCount),
        Arc::new(ToolRegistry::default()), TurnConfig::default(), Arc::new(EventBus::default()))
        .with_agents(agents, models)
        .with_provider_resolver(Default::default(), Arc::new(|_| Ok(Arc::new(EchoCount)))));
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();
    one_completed_turn(&sessions, &parent).await;
    let mut children = Vec::new();
    for route in ["one", "two", "missing"] {
        sessions.set_config(&parent, CallConfig { selection:Some(ModelSelection {route:route.into(), model:"expensive".into()}),
            reasoning:Some(Reasoning::Effort {effort:"high".into()}), temperature:Some(0.9), max_output_tokens:Some(9999), ..Default::default() }).unwrap();
        for mode in ["spawn", "fork"] {
            let request = SubagentRequest { parent:parent.clone(), agent:Some("scout".into()), prompt:"inspect".into() };
            if route == "missing" {
                let before = sessions.list().unwrap();
                assert!(rt.start(mode, request.clone()).await.unwrap_err().to_string().contains("no variant"));
                assert!(rt.start_continuable(mode, request).is_err());
                assert_eq!(before, sessions.list().unwrap());
                continue;
            }
            let run = rt.start(mode, request).await.unwrap();
            let config = sessions.config(&run.session).unwrap();
            assert_eq!(config.selection, Some(ModelSelection {route:route.into(), model:format!("cheap-{route}")}));
            assert_eq!(config.max_output_tokens, Some(if route == "one" {200} else {300}));
            assert_eq!(config.reasoning, None);
            assert_eq!(config.temperature, None);
            children.push((run.session, config));
        }
    }
    let reopened = service(dir.path());
    for (child, config) in children { assert_eq!(reopened.config(&child).unwrap(), config); }
}

#[tokio::test(flavor = "multi_thread")]
async fn named_roles_are_opt_in_and_inherit_generation_without_widening_tools() {
    use rness_engine::config::AgentDefinition;
    let dir = tempfile::tempdir().unwrap();
    let mut agents = std::collections::BTreeMap::new();
    for (name, subagent) in [("worker", true), ("principal", false)] {
        agents.insert(name.into(), AgentDefinition {
            subagent, description: name.into(), instructions: format!("Role {name}"),
            profile: None, tools: None, sandbox: None,
        });
    }
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()), Arc::new(EchoCount),
        Arc::new(ToolRegistry::default()), TurnConfig::default(), Arc::new(EventBus::default()),
    ).with_agents(agents, Default::default()));
    let rt = runtime(&sessions, 3);
    assert_eq!(rt.roster().keys().cloned().collect::<Vec<_>>(), vec!["worker"]);
    let parent = sessions.create(None).unwrap();
    sessions.select_agent(&parent, "principal").unwrap();
    let mut config = sessions.config(&parent).unwrap();
    config.max_output_tokens = Some(4096);
    config.agent.as_mut().unwrap().tools = Some(vec![]);
    sessions.set_config(&parent, config).unwrap();
    one_completed_turn(&sessions, &parent).await;
    for provider in ["spawn", "fork"] {
        let before = sessions.list().unwrap();
        for name in ["principal", "missing"] {
            let request = SubagentRequest { agent: Some(name.into()), parent: parent.clone(), prompt: "task".into() };
            assert!(rt.start(provider, request.clone()).await.is_err());
            assert!(rt.start_continuable(provider, request).is_err());
        }
        assert_eq!(sessions.list().unwrap(), before);
        let run = rt.start(provider, SubagentRequest {
            agent: Some("worker".into()), parent: parent.clone(), prompt: "task".into(),
        }).await.unwrap();
        let config = sessions.config(&run.session).unwrap();
        assert_eq!(config.agent.as_ref().unwrap().name, "worker");
        assert_eq!(config.max_output_tokens, Some(4096));
        assert_eq!(config.tool_ceiling, Some(vec![]));
        sessions.select_agent(&run.session, "principal").unwrap();
        sessions.set_config(&run.session, CallConfig::default()).unwrap();
        assert_eq!(sessions.config(&run.session).unwrap().tool_ceiling, Some(vec![]));
        assert_eq!(service(dir.path()).config(&run.session).unwrap().tool_ceiling, Some(vec![]));
        let child = rt.start_continuable(provider, SubagentRequest {
            agent: Some("worker".into()), parent: parent.clone(), prompt: "task".into(),
        }).unwrap();
        sessions.join(&child).await;
        assert_eq!(sessions.config(&child).unwrap().agent.unwrap().name, "worker");
    }
    let run = rt.start("spawn", SubagentRequest {
        agent: None, parent, prompt: "task".into(),
    }).await.unwrap();
    assert!(sessions.config(&run.session).unwrap().agent.is_none());
    let rt = rt.with_allow_generic(false);
    for provider in ["spawn", "fork"] {
        // The policy also applies when an existing child delegates again.
        let request = SubagentRequest {
            agent: None, parent: run.session.clone(), prompt: "task".into(),
        };
        let before = sessions.list().unwrap();
        let error = rt.start(provider, request.clone()).await.unwrap_err();
        assert!(error.to_string().contains("generic subagents are disabled"));
        assert!(rt.start_continuable(provider, request.clone()).is_err());
        assert_eq!(sessions.list().unwrap(), before);
        let named = SubagentRequest { agent: Some("worker".into()), ..request };
        rt.start(provider, named.clone()).await.unwrap();
        let child = rt.start_continuable(provider, named).unwrap();
        sessions.join(&child).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn spawn_child_is_fresh_and_settles_with_output() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();
    one_completed_turn(&sessions, &parent).await;

    let run = rt
        .start("spawn", SubagentRequest { agent: None, parent: parent.clone(), prompt: "tarea".into() })
        .await
        .unwrap();

    assert_eq!(run.stop, StopReason::Completed);
    // Fresh child: the model saw ONLY the delegated prompt.
    assert_eq!(run.output, "saw 1 turns");

    // The child is an ordinary session with delegation stamped.
    let d = sessions.store().delegation(&run.session).unwrap().unwrap();
    assert_eq!(d.parent, parent);
    assert_eq!(d.depth, 1);
    assert!(sessions.list().unwrap().contains(&run.session));
    // Spawn does NOT create branch lineage.
    assert!(sessions.store().parent(&run.session).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn fork_child_inherits_completed_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();
    one_completed_turn(&sessions, &parent).await;

    let run = rt
        .start("fork", SubagentRequest { agent: None, parent: parent.clone(), prompt: "sigue".into() })
        .await
        .unwrap();

    assert_eq!(run.stop, StopReason::Completed);
    // Inherited: parent's user+assistant plus the delegated prompt.
    assert_eq!(run.output, "saw 3 turns");
    // Fork creates BOTH lineages: branch (fork ref) and delegation.
    assert!(sessions.store().parent(&run.session).unwrap().is_some());
    assert_eq!(sessions.store().delegation(&run.session).unwrap().unwrap().depth, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn fork_of_turnless_parent_degrades_to_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();
    // No completed turn in the parent: nothing replayable to inherit.
    let run = rt
        .start("fork", SubagentRequest { agent: None, parent, prompt: "solo".into() })
        .await
        .unwrap();
    assert_eq!(run.output, "saw 1 turns");
    assert!(sessions.store().parent(&run.session).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn depth_is_enforced_from_durable_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 1);
    let parent = sessions.create(None).unwrap();

    let run = rt
        .start("spawn", SubagentRequest { agent: None, parent, prompt: "nivel 1".into() })
        .await
        .unwrap();

    // The child delegating again would be depth 2 > max 1 — refused,
    // even through a FRESH runtime (depth lives in the log, not memory).
    let rt2 = runtime(&sessions, 1);
    let err = rt2
        .start("spawn", SubagentRequest { agent: None, parent: run.session, prompt: "nivel 2".into() })
        .await
        .unwrap_err();
    assert!(matches!(err, SubagentError::DepthExceeded { depth: 2, max: 1 }));
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_provider_fails_loud() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();
    let err = rt
        .start("acp", SubagentRequest { agent: None, parent, prompt: "x".into() })
        .await
        .unwrap_err();
    assert!(matches!(err, SubagentError::UnknownProvider(_)));
}

// -- continuable children ----------------------------------------------------

use rness_engine::subagent::SubagentError as SErr;
use rness_protocol::branch::DelegationMode;

#[tokio::test(flavor = "multi_thread")]
async fn continuable_child_starts_and_accepts_messages() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();

    let child = rt
        .start_continuable("spawn", SubagentRequest { agent: None, parent: parent.clone(), prompt: "trabaja".into() })
        .unwrap();

    // Continuable mode is stamped durably.
    let d = sessions.store().delegation(&child).unwrap().unwrap();
    assert_eq!(d.mode, DelegationMode::Continuable);
    assert_eq!(d.parent, parent);

    sessions.join(&child).await;

    // Parent -> child follow-up is accepted (idle target starts a turn).
    rt.send_message(&parent, &child, "sigue".into()).unwrap();
    sessions.join(&child).await;

    // The framed agent message is in the child's log.
    let history = sessions.store().history(&child).unwrap();
    let framed = history.iter().any(|e| match &e.event {
        SessionEvent::UserMessage(m) => m.content.iter().any(|p| match p {
            ContentPart::Text { text } => text.starts_with(&format!("Agent {parent} sent a message:")),
            _ => false,
        }),
        _ => false,
    });
    assert!(framed, "framed agent message committed to the child log");
}

#[tokio::test(flavor = "multi_thread")]
async fn settle_notice_reaches_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();

    let child = rt
        .start_continuable("spawn", SubagentRequest { agent: None, parent: parent.clone(), prompt: "hola".into() })
        .unwrap();
    sessions.join(&child).await;

    // Delivery is asynchronous: wait for the woken turn to finish.
    wait_for_parent_turns(&sessions, &parent, 1).await;
    sessions.join(&parent).await;
    let history = sessions.store().history(&parent).unwrap();
    assert_eq!(history.iter().filter(|e| matches!(e.event, SessionEvent::TurnStarted { .. })).count(), 1);
    assert!(history.iter().any(|e| matches!(e.event, SessionEvent::AssistantMessage(_))));
    let notice = history.iter().any(|e| match &e.event {
        SessionEvent::UserMessage(m) => {
            m.intent == UserIntent::Followup
                && m.content.iter().any(|p| match p {
                    ContentPart::Text { text } => {
                        text.starts_with(&format!("[subagent {child} settled: completed]"))
                            && text.contains("saw 1 turns")
                    }
                    _ => false,
                })
        }
        _ => false,
    });
    assert!(notice, "settle notice wakes parent with child output");
    let _ = rt;
}

#[tokio::test(flavor = "multi_thread")]
async fn message_authority_is_exact_adjacency() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();
    let stranger = sessions.create(None).unwrap();

    let child = rt
        .start_continuable("spawn", SubagentRequest { agent: None, parent: parent.clone(), prompt: "x".into() })
        .unwrap();
    sessions.join(&child).await;

    // A stranger cannot message the child.
    assert!(matches!(
        rt.send_message(&stranger, &child, "hola".into()),
        Err(SErr::NotAuthorized(_))
    ));

    wait_for_parent_turns(&sessions, &parent, 1).await;

    // The child CAN message its direct parent (up edge).
    rt.send_message(&child, &parent, "reporte".into()).unwrap();
    sessions.join(&parent).await;

    // A one-shot child cannot accept messages at all.
    let run = rt
        .start("spawn", SubagentRequest { agent: None, parent: parent.clone(), prompt: "una vez".into() })
        .await
        .unwrap();
    assert!(matches!(
        rt.send_message(&parent, &run.session, "otra".into()),
        Err(SErr::NotAuthorized(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_requires_ancestry_and_noops_when_idle() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();
    let stranger = sessions.create(None).unwrap();

    let child = rt
        .start_continuable("spawn", SubagentRequest { agent: None, parent: parent.clone(), prompt: "x".into() })
        .unwrap();
    sessions.join(&child).await;

    // Idle interrupt from the parent: accepted no-op.
    rt.interrupt(&parent, &child).unwrap();
    // Stranger: refused.
    assert!(matches!(rt.interrupt(&stranger, &child), Err(SErr::NotAuthorized(_))));
    // Self: refused (not an ancestor of itself).
    assert!(matches!(rt.interrupt(&child, &child), Err(SErr::NotAuthorized(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_children_shows_continuable_only_in_preorder() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();

    let c1 = rt
        .start_continuable("spawn", SubagentRequest { agent: None, parent: parent.clone(), prompt: "a".into() })
        .unwrap();
    sessions.join(&c1).await;
    // One-shot sibling: must be invisible to discovery.
    let one_shot = rt
        .start("spawn", SubagentRequest { agent: None, parent: parent.clone(), prompt: "b".into() })
        .await
        .unwrap();
    // Grandchild under c1.
    let g1 = rt
        .start_continuable("spawn", SubagentRequest { agent: None, parent: c1.clone(), prompt: "c".into() })
        .unwrap();
    sessions.join(&g1).await;

    // The CLI enables persisted jobs under the same session root.
    std::fs::create_dir_all(dir.path().join("jobs/job-1")).unwrap();
    std::fs::write(dir.path().join("jobs/job-1/state.json"), "{}").unwrap();
    let direct = rt.list_children(&parent, false).unwrap();
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].session, c1);
    assert_eq!(direct[0].depth, 1);

    let all = rt.list_children(&parent, true).unwrap();
    let ids: Vec<_> = all.iter().map(|c| c.session.clone()).collect();
    assert_eq!(ids, vec![c1.clone(), g1.clone()], "pre-order: child then its subtree");
    assert!(!ids.contains(&one_shot.session), "one-shot children are absent");
}

#[tokio::test(flavor = "multi_thread")]
async fn continuable_depth_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 1);
    let parent = sessions.create(None).unwrap();
    let child = rt
        .start_continuable("spawn", SubagentRequest { agent: None, parent, prompt: "x".into() })
        .unwrap();
    sessions.join(&child).await;
    let err = rt
        .start_continuable("spawn", SubagentRequest { agent: None, parent: child, prompt: "y".into() })
        .unwrap_err();
    assert!(matches!(err, SErr::DepthExceeded { depth: 2, max: 1 }));
}

#[tokio::test(flavor = "multi_thread")]
async fn every_settle_reaches_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let parent = sessions.create(None).unwrap();

    let child = rt
        .start_continuable("spawn", SubagentRequest { agent: None, parent: parent.clone(), prompt: "uno".into() })
        .unwrap();
    sessions.join(&child).await;
    wait_for_parent_turns(&sessions, &parent, 1).await;
    sessions.join(&parent).await;

    rt.send_message(&parent, &child, "dos".into()).unwrap();
    sessions.join(&child).await;
    wait_for_parent_turns(&sessions, &parent, 2).await;
    sessions.join(&parent).await;

    let history = sessions.store().history(&parent).unwrap();
    let notices = history
        .iter()
        .filter(|e| match &e.event {
            SessionEvent::UserMessage(m) => m.content.iter().any(|p| match p {
                ContentPart::Text { text } => text.contains("settled"),
                _ => false,
            }),
            _ => false,
        })
        .count();
    assert_eq!(notices, 2, "one settle notice per child turn");
    let _ = rt;
}

/// Blocks in its first step until the test releases it — the window in
/// which a "late steer" (settle notice) can arrive after the last
/// boundary drain.
struct GatedStep {
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<()>>,
}

#[async_trait]
impl Provider for GatedStep {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, request: StepRequest<'_>, _cancel: &CancellationToken) -> StepOutcome {
        if request.context.turns.len() == 1 {
            let _ = self.entered.send(());
            let _ = self.release.lock().await.recv().await;
        }
        StepOutcome::Committed(AssistantMessage {
            model: "fake-1".into(),
            content: vec![ContentPart::Text { text: format!("turno con {} entradas", request.context.turns.len()) }],
            stop: StopReason2::EndTurn,
            usage: Usage::default(),
            estimated_input: 0,
            chunks: vec![],
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn steer_accepted_during_final_step_is_not_lost() {
    let dir = tempfile::tempdir().unwrap();
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = tokio::sync::mpsc::unbounded_channel();
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(GatedStep { entered: entered_tx, release: tokio::sync::Mutex::new(release_rx) }),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let id = sessions.create(None).unwrap();

    sessions
        .send(&id, UserIntent::Followup, vec![ContentPart::Text { text: "hola".into() }])
        .unwrap();
    // The provider is inside the FINAL step (boundary drains already done).
    entered_rx.recv().await.unwrap();
    // A steer lands now — before the fix it parked in memory forever.
    sessions
        .send(&id, UserIntent::Steer, vec![ContentPart::Text { text: "tarde".into() }])
        .unwrap();
    release_tx.send(()).unwrap();
    sessions.join(&id).await;

    let history = sessions.store().history(&id).unwrap();
    let committed = history.iter().any(|e| match &e.event {
        SessionEvent::UserMessage(m) => m.content.iter().any(|p| matches!(p, ContentPart::Text { text } if text == "tarde")),
        _ => false,
    });
    assert!(committed, "late steer committed to the log");
    // And it ran as its own turn (steer degrades to followup post-turn).
    let turns = history
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::TurnStarted { .. }))
        .count();
    assert_eq!(turns, 2, "the late steer started its own turn");
}

async fn wait_for_notices(sessions: &SessionService, parent: &SessionId, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let history = sessions.store().history(parent).unwrap();
            let notices = history.iter().filter(|e| matches!(&e.event,
                SessionEvent::UserMessage(m) if m.content.iter().any(|p|
                    matches!(p, ContentPart::Text { text } if text.contains("settled")))))
                .count();
            if notices >= count { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("settlement delivered");
}

#[tokio::test(flavor = "multi_thread")]
async fn settlements_wake_repeatedly_and_interrupt_is_not_teardown() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let parent = sessions.create(None).unwrap();
    // Earlier background completions must not prevent later child wakes.
    for _ in 0..3 {
        sessions.notify_job(&parent, "job done".into()).unwrap();
        sessions.join(&parent).await;
    }
    sessions.cancel(&parent);
    for _ in 0..4 {
        assert_eq!(sessions.notify_subagent_settled(&parent, "child settled".into()).await.unwrap(),
            rness_engine::inbox::Disposition::StartTurn);
        sessions.join(&parent).await;
    }
    let history = sessions.store().history(&parent).unwrap();
    assert_eq!(history.iter().filter(|e| matches!(e.event, SessionEvent::TurnStarted { .. })).count(), 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn teardown_logs_child_output_without_waking_parent_or_ancestor() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let rt = runtime(&sessions, 3);
    let root = sessions.create(None).unwrap();
    sessions.begin_teardown(&root);
    let child = rt.start_continuable("spawn", SubagentRequest {
        agent: None, parent: root.clone(), prompt: "inspect".into(),
    }).unwrap();
    sessions.join(&child).await;
    wait_for_notices(&sessions, &root, 1).await;
    let history = sessions.store().history(&root).unwrap();
    assert!(!history.iter().any(|e| matches!(e.event, SessionEvent::TurnStarted { .. })));
    assert!(history.iter().any(|e| matches!(&e.event, SessionEvent::UserMessage(m)
        if m.intent == UserIntent::Inject)));
    assert_eq!(sessions.notify_subagent_settled(&child, "grandchild settled".into()).await.unwrap(),
        rness_engine::inbox::Disposition::LogOnly);
    let history = sessions.store().history(&child).unwrap();
    assert_eq!(history.iter().filter(|e| matches!(e.event, SessionEvent::TurnStarted { .. })).count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn busy_parent_batches_late_child_settlements() {
    let dir = tempfile::tempdir().unwrap();
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = tokio::sync::mpsc::unbounded_channel();
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(GatedStep { entered: entered_tx, release: tokio::sync::Mutex::new(release_rx) }),
        Arc::new(ToolRegistry::default()), TurnConfig::default(), Arc::new(EventBus::default()),
    ));
    let parent = sessions.create(None).unwrap();
    sessions.send(&parent, UserIntent::Followup, vec![ContentPart::Text { text: "work".into() }]).unwrap();
    entered_rx.recv().await.unwrap();
    for i in 0..2 {
        assert_eq!(sessions.notify_subagent_settled(&parent, format!("child {i} settled")).await.unwrap(),
            rness_engine::inbox::Disposition::Queued);
    }
    release_tx.send(()).unwrap();
    sessions.join(&parent).await;
    let history = sessions.store().history(&parent).unwrap();
    assert_eq!(history.iter().filter(|e| matches!(e.event, SessionEvent::TurnStarted { .. })).count(), 2);
    assert!(history.iter().any(|e| matches!(&e.event, SessionEvent::AssistantMessage(m)
        if m.content.iter().any(|p| matches!(p, ContentPart::Text { text } if text == "turno con 4 entradas")))));
}

async fn wait_for_parent_turns(sessions: &SessionService, parent: &SessionId, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let history = sessions.store().history(parent).unwrap();
            if history.iter().filter(|e| matches!(e.event, SessionEvent::TurnEnded { .. })).count() >= count
                && sessions.phase(parent) == rness_engine::inbox::Phase::Idle { break; }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }).await.expect("parent processed settlements");
}

#[tokio::test(flavor = "multi_thread")]
async fn settlement_waits_for_maintenance_then_observes_teardown() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let parent = sessions.create(None).unwrap();
    let maintenance = sessions.try_extension_maintenance().unwrap();
    let delivery = sessions.notify_subagent_settled(&parent, "child settled".into());
    tokio::pin!(delivery);
    assert!(tokio::time::timeout(std::time::Duration::from_millis(20), &mut delivery).await.is_err());
    sessions.begin_teardown(&parent);
    drop(maintenance);
    assert_eq!(delivery.await.unwrap(), rness_engine::inbox::Disposition::LogOnly);
    assert!(!sessions.store().history(&parent).unwrap().iter()
        .any(|e| matches!(e.event, SessionEvent::TurnStarted { .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn settlement_during_running_teardown_is_logged_after_writer_retires() {
    let dir = tempfile::tempdir().unwrap();
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = tokio::sync::mpsc::unbounded_channel();
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(GatedStep { entered: entered_tx, release: tokio::sync::Mutex::new(release_rx) }),
        Arc::new(ToolRegistry::default()), TurnConfig::default(), Arc::new(EventBus::default()),
    ));
    let parent = sessions.create(None).unwrap();
    sessions.send(&parent, UserIntent::Followup, vec![ContentPart::Text { text: "work".into() }]).unwrap();
    entered_rx.recv().await.unwrap();
    sessions.begin_teardown(&parent);
    let delivery = sessions.notify_subagent_settled(&parent, "child settled".into());
    tokio::pin!(delivery);
    assert!(tokio::time::timeout(std::time::Duration::from_millis(20), &mut delivery).await.is_err());
    release_tx.send(()).unwrap();
    assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(5), delivery).await.unwrap().unwrap(),
        rness_engine::inbox::Disposition::LogOnly);
    let history = sessions.store().history(&parent).unwrap();
    assert_eq!(history.iter().filter(|e| matches!(e.event, SessionEvent::TurnStarted { .. })).count(), 1);
    assert!(matches!(&history.last().unwrap().event, SessionEvent::UserMessage(m)
        if m.intent == UserIntent::Inject));
}
