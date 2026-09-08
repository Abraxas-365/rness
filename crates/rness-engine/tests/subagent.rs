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
    let rt = SubagentRuntime::new(Arc::clone(sessions), max_depth);
    rt.register(Arc::new(SpawnProvider));
    rt.register(Arc::new(ForkProvider));
    rt
}

/// Run one parent turn so there is a completed prefix to inherit.
async fn one_completed_turn(sessions: &Arc<SessionService>, id: &SessionId) {
    sessions
        .send(id, UserIntent::Followup, vec![ContentPart::Text { text: "hola".into() }])
        .unwrap();
    sessions.join(id).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn named_roles_are_opt_in_and_inherit_generation_without_widening_tools() {
    use rness_engine::config::AgentDefinition;
    let dir = tempfile::tempdir().unwrap();
    let mut agents = std::collections::BTreeMap::new();
    for (name, subagent) in [("worker", true), ("principal", false)] {
        agents.insert(name.into(), AgentDefinition {
            subagent, description: name.into(), instructions: format!("Role {name}"),
            profile: None, tools: None,
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

    // The runtime's settle watch injected a notice into the idle parent.
    let history = sessions.store().history(&parent).unwrap();
    let notice = history.iter().any(|e| match &e.event {
        SessionEvent::UserMessage(m) => {
            m.intent == UserIntent::Inject
                && m.content.iter().any(|p| match p {
                    ContentPart::Text { text } => {
                        text.starts_with(&format!("[subagent {child} settled: completed]"))
                    }
                    _ => false,
                })
        }
        _ => false,
    });
    assert!(notice, "settle notice injected into the parent log");
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
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    rt.send_message(&parent, &child, "dos".into()).unwrap();
    sessions.join(&child).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

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
