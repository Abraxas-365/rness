//! Durable sandbox policy at the service and turn boundaries (no OS backend required).

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use rness_engine::config::{AgentDefinition, ModelRegistry, Profile};
use rness_engine::sandbox::{SandboxConfig, SandboxMode};
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::subagent::{ForkProvider, SpawnProvider, SubagentRequest, SubagentRuntime};
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::{run_turn, TurnConfig, TurnError};
use rness_kernel::EventBus;
use rness_protocol::branch::{Delegation, DelegationMode};
use rness_protocol::events::*;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Answer(AtomicUsize);

#[async_trait]
impl Provider for Answer {
    fn model(&self) -> &str {
        "test"
    }

    async fn step(&self, _: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        StepOutcome::Committed(AssistantMessage {
            model: "test".into(),
            content: vec![ContentPart::Text {
                text: "done".into(),
            }],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
            estimated_input: 0,
            chunks: vec![],
        })
    }
}

fn service(path: &std::path::Path) -> SessionService {
    SessionService::new(
        SessionStore::new(path),
        Arc::new(Answer::default()),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    )
}

fn policy(mode: SandboxMode) -> CallConfig {
    CallConfig {
        sandbox: Some(mode),
        ..Default::default()
    }
}

fn agent(mode: Option<SandboxMode>, profile: bool) -> AgentDefinition {
    AgentDefinition {
        subagent: true,
        description: "Test role".into(),
        instructions: "Answer".into(),
        profile: profile.then(|| "small".into()),
        tools: None,
        sandbox: mode,
    }
}

fn roles(service: SessionService) -> SessionService {
    let mut models = ModelRegistry::default();
    models
        .declare_profile(
            "small".into(),
            Profile::Fixed {
                provider: "test".into(),
                model: "small".into(),
                options: Default::default(),
            },
        )
        .unwrap();
    service
        .with_agents(
            [
                ("plain".into(), agent(None, false)),
                ("profile".into(), agent(None, true)),
                (
                    "writer".into(),
                    agent(Some(SandboxMode::WorkspaceWrite), false),
                ),
                (
                    "profile-writer".into(),
                    agent(Some(SandboxMode::WorkspaceWrite), true),
                ),
                ("reader".into(), agent(Some(SandboxMode::ReadOnly), false)),
            ]
            .into(),
            models,
        )
        .with_provider_resolver(
            CallConfig::default(),
            Arc::new(|_| Ok(Arc::new(Answer::default()))),
        )
}

#[test]
fn defaults_remain_opt_in_even_with_a_named_profile() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = roles(service(dir.path()));
    let id = sessions.create(None).unwrap();
    assert_eq!(sessions.config(&id).unwrap(), CallConfig::default());
    assert!(!sessions
        .store()
        .history(&id)
        .unwrap()
        .iter()
        .any(|entry| matches!(entry.event, SessionEvent::RequestConfig(_))));
    sessions.set_config(&id, CallConfig::default()).unwrap();
    for name in ["plain", "profile"] {
        sessions.select_agent(&id, name).unwrap();
        assert_eq!(sessions.config(&id).unwrap().sandbox, None);
    }
}

#[test]
fn reopening_with_a_restricted_default_does_not_change_absent_durable_policy() {
    for default in [SandboxMode::WorkspaceWrite, SandboxMode::ReadOnly] {
        let dir = tempfile::tempdir().unwrap();
        let original = service(dir.path());
        let id = original.create(None).unwrap();
        drop(original);

        let reopened = roles(service(dir.path())).with_sandbox(SandboxConfig {
            default,
            ..Default::default()
        });
        reopened.set_config(&id, CallConfig::default()).unwrap();
        let profile = reopened.profile_config("small", None).unwrap();
        reopened.set_config(&id, profile).unwrap();
        let config = reopened.config(&id).unwrap();
        assert_eq!(config.sandbox, None);
        assert_eq!(config.profile.as_deref(), Some("small"));
        assert_eq!(reopened.replay(&id).unwrap().context.config.sandbox, None);
        drop(reopened);
        assert_eq!(service(dir.path()).config(&id).unwrap().sandbox, None);

        // In contrast, new sessions still opt into the current startup default.
        let current = service(dir.path()).with_sandbox(SandboxConfig {
            default,
            ..Default::default()
        });
        assert!(current.create(None).is_err());
        let workspace = tempfile::tempdir().unwrap();
        let fresh = current
            .create(Some(workspace.path().display().to_string()))
            .unwrap();
        assert_eq!(current.config(&fresh).unwrap().sandbox, Some(default));
    }
}

#[test]
fn omitted_sandbox_survives_updates_forks_and_restart() {
    for default in [SandboxMode::DangerFullAccess, SandboxMode::WorkspaceWrite] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let sessions = service(dir.path()).with_sandbox(SandboxConfig {
            default,
            ..Default::default()
        });
        let id = sessions
            .create(Some(workspace.path().display().to_string()))
            .unwrap();
        if default == SandboxMode::DangerFullAccess {
            sessions
                .set_config(&id, policy(SandboxMode::WorkspaceWrite))
                .unwrap();
        }
        sessions
            .set_config(
                &id,
                CallConfig {
                    temperature: Some(0.4),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            sessions.config(&id).unwrap().sandbox,
            Some(SandboxMode::WorkspaceWrite)
        );
        sessions
            .set_config(&id, policy(SandboxMode::ReadOnly))
            .unwrap();
        sessions.set_config(&id, CallConfig::default()).unwrap();
        let before = sessions.store().history(&id).unwrap().len();
        sessions.set_config(&id, CallConfig::default()).unwrap();
        for mode in [SandboxMode::WorkspaceWrite, SandboxMode::DangerFullAccess] {
            assert!(sessions
                .set_config(&id, policy(mode))
                .unwrap_err()
                .to_string()
                .contains("cannot be broadened"));
        }
        assert_eq!(sessions.store().history(&id).unwrap().len(), before);
        let child = sessions.fork(&id, None).unwrap();
        drop(sessions);
        let reopened = service(dir.path());
        for session in [&id, &child] {
            assert_eq!(
                reopened.config(session).unwrap().sandbox,
                Some(SandboxMode::ReadOnly)
            );
            assert_eq!(
                reopened.replay(session).unwrap().context.config.sandbox,
                Some(SandboxMode::ReadOnly)
            );
        }
    }
}

#[test]
fn agent_selection_and_profiles_only_tighten() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let sessions = roles(service(dir.path()));
    let id = sessions
        .create(Some(workspace.path().display().to_string()))
        .unwrap();
    sessions.select_agent(&id, "writer").unwrap();
    assert_eq!(
        sessions.config(&id).unwrap().sandbox,
        Some(SandboxMode::WorkspaceWrite)
    );
    sessions.select_agent(&id, "reader").unwrap();
    for name in ["plain", "profile", "writer", "profile-writer"] {
        sessions.select_agent(&id, name).unwrap();
        let config = sessions.config(&id).unwrap();
        assert_eq!(config.sandbox, Some(SandboxMode::ReadOnly), "{name}");
        assert_eq!(config.agent.unwrap().name, name);
    }
    let profile = sessions.profile_config("small", None).unwrap();
    sessions.set_config(&id, profile).unwrap();
    assert_eq!(
        sessions.config(&id).unwrap().sandbox,
        Some(SandboxMode::ReadOnly)
    );
}

#[test]
fn creation_checks_resolved_seed_before_writing_and_retains_global_restriction() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = service(dir.path()).with_provider_resolver(
        policy(SandboxMode::ReadOnly),
        Arc::new(|_| Ok(Arc::new(Answer::default()))),
    );
    assert!(sessions
        .create(None)
        .unwrap_err()
        .to_string()
        .contains("workspace"));
    assert!(sessions.list().unwrap().is_empty());
    let workspace = tempfile::tempdir().unwrap();
    let id = sessions
        .create(Some(workspace.path().display().to_string()))
        .unwrap();
    assert_eq!(
        sessions.config(&id).unwrap().sandbox,
        Some(SandboxMode::ReadOnly)
    );

    let sessions = service(dir.path())
        .with_sandbox(SandboxConfig {
            default: SandboxMode::WorkspaceWrite,
            ..Default::default()
        })
        .with_provider_resolver(
            policy(SandboxMode::DangerFullAccess),
            Arc::new(|_| Ok(Arc::new(Answer::default()))),
        );
    assert!(sessions.create(None).is_err());
    let id = sessions
        .create(Some(workspace.path().display().to_string()))
        .unwrap();
    assert_eq!(
        sessions.config(&id).unwrap().sandbox,
        Some(SandboxMode::WorkspaceWrite)
    );
}

#[test]
fn delegated_creation_itself_retains_parent_policy() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let sessions = service(dir.path());
    let parent = sessions
        .create(Some(workspace.path().display().to_string()))
        .unwrap();
    sessions
        .set_config(&parent, policy(SandboxMode::ReadOnly))
        .unwrap();
    let child = sessions
        .create_delegated(
            None,
            Delegation {
                parent,
                call: None,
                depth: 1,
                mode: DelegationMode::OneShot,
            },
        )
        .unwrap();
    assert_eq!(
        sessions.config(&child).unwrap().sandbox,
        Some(SandboxMode::ReadOnly)
    );
}

fn historical_fork_retains_current_policy(delegated: bool) {
    for sandbox in [None, Some(SandboxMode::WorkspaceWrite)] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let sessions = service(dir.path());
        let parent = sessions
            .create(Some(workspace.path().display().to_string()))
            .unwrap();
        sessions
            .set_config(
                &parent,
                CallConfig {
                    sandbox,
                    temperature: Some(0.2),
                    ..Default::default()
                },
            )
            .unwrap();
        let at = sessions
            .store()
            .history(&parent)
            .unwrap()
            .last()
            .unwrap()
            .id
            .clone();
        sessions
            .set_config(
                &parent,
                CallConfig {
                    sandbox: Some(SandboxMode::ReadOnly),
                    temperature: Some(0.8),
                    ..Default::default()
                },
            )
            .unwrap();
        let before = sessions.store().history(&parent).unwrap().len();
        let child = if delegated {
            sessions
                .fork_delegated(
                    &parent,
                    Some(at),
                    Delegation {
                        parent: parent.clone(),
                        call: None,
                        depth: 1,
                        mode: DelegationMode::OneShot,
                    },
                )
                .unwrap()
        } else {
            sessions.fork(&parent, Some(at)).unwrap()
        };
        let config = sessions.config(&child).unwrap();
        assert_eq!(config.sandbox, Some(SandboxMode::ReadOnly));
        assert_eq!(
            config.temperature,
            Some(0.2),
            "retain historical request options"
        );
        assert_eq!(sessions.store().history(&parent).unwrap().len(), before);
        assert!(sessions
            .set_config(&child, policy(SandboxMode::WorkspaceWrite))
            .is_err());
        drop(sessions);
        let reopened = service(dir.path());
        assert_eq!(
            reopened.config(&child).unwrap().sandbox,
            Some(SandboxMode::ReadOnly)
        );
        assert_eq!(
            reopened.replay(&child).unwrap().context.config.sandbox,
            Some(SandboxMode::ReadOnly)
        );
    }
}

#[test]
fn historical_ordinary_fork_retains_current_parent_restriction() {
    historical_fork_retains_current_policy(false);
}

#[test]
fn historical_delegated_fork_retains_current_parent_restriction() {
    historical_fork_retains_current_policy(true);
}

#[tokio::test]
async fn named_and_unnamed_spawn_and_fork_inherit_parent_restriction() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let sessions = Arc::new(roles(service(dir.path())));
    let runtime = SubagentRuntime::new(sessions.clone(), 3).with_allow_generic(true);
    runtime.register(Arc::new(SpawnProvider));
    runtime.register(Arc::new(ForkProvider));
    let parent = sessions
        .create(Some(workspace.path().display().to_string()))
        .unwrap();
    sessions
        .set_config(&parent, policy(SandboxMode::ReadOnly))
        .unwrap();
    sessions
        .send(
            &parent,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "hello".into(),
            }],
        )
        .unwrap();
    sessions.join(&parent).await;
    let mut children = vec![];
    for provider in ["spawn", "fork"] {
        for name in [
            None,
            Some("plain"),
            Some("profile"),
            Some("writer"),
            Some("profile-writer"),
        ] {
            let run = runtime
                .start(
                    provider,
                    SubagentRequest {
                        parent: parent.clone(),
                        agent: name.map(str::to_owned),
                        prompt: "answer".into(),
                    },
                )
                .await
                .unwrap();
            let config = sessions.config(&run.session).unwrap();
            assert_eq!(
                config.sandbox,
                Some(SandboxMode::ReadOnly),
                "{provider} {name:?}"
            );
            assert_eq!(config.agent.as_ref().map(|a| a.name.as_str()), name);
            if name.is_some_and(|n| n.starts_with("profile")) {
                assert_eq!(config.selection.unwrap().model, "small");
            }
            children.push(run.session);
        }
    }
    let reopened = service(dir.path());
    for child in children {
        assert_eq!(
            reopened.config(&child).unwrap().sandbox,
            Some(SandboxMode::ReadOnly)
        );
    }
}

#[tokio::test]
async fn restricted_turn_without_workspace_fails_before_provider_and_logs_failure() {
    for sandbox in [
        None,
        Some(SandboxMode::DangerFullAccess),
        Some(SandboxMode::WorkspaceWrite),
        Some(SandboxMode::ReadOnly),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        // Bypass service admission to exercise old/imported logs and direct callers.
        let mut log = store.create(None).unwrap();
        log.append(&SessionEvent::RequestConfig(CallConfig {
            sandbox,
            ..Default::default()
        }))
        .unwrap();
        log.append(&SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text {
                text: "hello".into(),
            }],
            source: None,
        }))
        .unwrap();
        let provider = Answer::default();
        let result = run_turn(
            &store,
            &mut log,
            &provider,
            &ToolRegistry::default(),
            &TurnConfig::default(),
            &CancellationToken::new(),
            &mut || vec![],
            1,
            &|_| {},
        )
        .await;
        let restricted = sandbox.is_some_and(|mode| mode != SandboxMode::DangerFullAccess);
        if restricted {
            assert!(matches!(result, Err(TurnError::SandboxWorkspaceRequired)));
        } else {
            assert_eq!(result.unwrap(), TurnOutcome::Completed);
        }
        assert_eq!(provider.0.load(Ordering::SeqCst), usize::from(!restricted));
        let history = log.read_all().unwrap();
        assert!(matches!(&history.last().unwrap().event,
            SessionEvent::TurnEnded { outcome, .. }
                if *outcome == if restricted { TurnOutcome::Failed } else { TurnOutcome::Completed }));
    }
}
