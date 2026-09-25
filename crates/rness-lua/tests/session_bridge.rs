//! rness.session end-to-end: a Lua plugin drives the engine — sends a
//! prompt, the scripted provider answers, Lua reads the transcript and
//! forks. The whole chain crosses the VM actor thread.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::tools::ToolRegistry;
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
            content: vec![ContentPart::Text {
                text: "hola desde el modelo".into(),
            }],
            stop: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
            estimated_input: 0,
            chunks: vec![],
        })
    }
}

#[tokio::test]
async fn questions_transactional_lifecycle_and_pending_unload() {
    use rness_engine::questions::{QuestionEvent, Questions};
    use rness_engine::tools::ToolCall;
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(OneAnswer),
        registry.clone(),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let subagents = Arc::new(rness_engine::subagent::SubagentRuntime::new(
        sessions.clone(),
        3,
    ));
    let host = rness_lua::plugin_host::LuaHost::spawn().unwrap();
    host.install_session(
        sessions,
        subagents,
        registry.clone(),
        Default::default(),
        tokio::runtime::Handle::current(),
        "fake-1".into(),
    )
    .await
    .unwrap();
    let qs = Arc::new(Questions::default());
    qs.bind_registry(&registry);
    host.install_questions(qs.clone()).await.unwrap();
    assert!(!qs.is_available());
    assert!(registry.get("AskUser").is_none());
    assert!(host
        .load("broken", "rness.questions.enable(); error('broken')")
        .await
        .is_err());
    assert!(!qs.is_available());
    assert!(registry.get("AskUser").is_none());
    host.load(
        "owner",
        "rness.questions.enable {priority=123, title='Decisions'}",
    )
    .await
    .unwrap();
    assert_eq!(qs.overlay_config().priority, 123);
    let mut events = qs.subscribe();
    let tools = registry.clone();
    let task = tokio::spawn(async move {
        tools
            .dispatch(
                &"s".into(),
                &[ToolCall {
                    call: "c".into(),
                    name: "AskUser".into(),
                    args: serde_json::json!({"questions":[{"id":"q","question":"Choose"}]}),
                }],
                1,
                &CancellationToken::new(),
            )
            .await
    });
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap(),
        QuestionEvent::QuestionRequested { .. }
    ));
    assert!(host
        .load(
            "failed-disable",
            "rness.questions.disable(); error('broken')"
        )
        .await
        .is_err());
    assert_eq!(qs.pending().len(), 1);
    assert!(host
        .unload_coordinated("owner", vec![], |_| {})
        .await
        .unwrap());
    assert!(task.await.unwrap()[0].is_error);
    assert!(matches!(
        events.recv().await.unwrap(),
        QuestionEvent::QuestionResolved { .. }
    ));
    assert!(qs.pending().is_empty());
    assert!(!qs.is_available());
    assert!(registry.get("AskUser").is_none());
    host.load("disabled", "rness.questions.enable {enabled=false}")
        .await
        .unwrap();
    assert!(!qs.is_available());
    assert!(registry.get("AskUser").is_none());
    host.load("next", "rness.questions.enable()").await.unwrap();
    host.reload(vec![]).await.unwrap();
    assert!(!qs.is_available());
    assert!(registry.get("AskUser").is_none());
    assert!(qs.owner().is_none());
}

struct BlockingCommand {
    entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl rness_engine::interaction::Command for BlockingCommand {
    fn name(&self) -> &str {
        "blocked"
    }
    fn description(&self) -> &str {
        "Test running command reservation"
    }
    fn execute(
        &self,
        _: &SessionService,
        _: rness_engine::interaction::CommandInvocation<'_>,
    ) -> Result<rness_engine::interaction::CommandResult, rness_engine::service::ServiceError> {
        self.entered
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(())
            .unwrap();
        self.release.lock().unwrap().recv().unwrap();
        Ok(Default::default())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn model_plugin_updates_session_settings_durably() {
    use rness_engine::config::{
        ModelCapabilities, ModelDeclaration, ModelRegistry, Profile, ReasoningCapabilities,
        RequestOptions,
    };
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let mut models = ModelRegistry::default();
    models
        .declare_model(ModelDeclaration {
            provider: "test".into(),
            model: "fake-1".into(),
            capabilities: ModelCapabilities {
                temperature: Some(true),
                max_output_tokens: Some(4096),
                reasoning: Some(ReasoningCapabilities {
                    efforts: Some(vec!["high".into()]),
                    budget_tokens: None,
                }),
                ..Default::default()
            },
        })
        .unwrap();
    models
        .declare_model(ModelDeclaration {
            provider: "test".into(),
            model: "restricted".into(),
            capabilities: ModelCapabilities {
                temperature: Some(false),
                output_token_limit: Some(false),
                ..Default::default()
            },
        })
        .unwrap();
    models
        .declare_profile(
            "restricted".into(),
            Profile::Fixed {
                provider: "test".into(),
                model: "restricted".into(),
                options: RequestOptions::default(),
            },
        )
        .unwrap();
    let sessions = Arc::new(
        SessionService::new(
            SessionStore::new(dir.path()),
            Arc::new(OneAnswer),
            registry.clone(),
            TurnConfig::default(),
            Arc::new(EventBus::default()),
        )
        .with_agents(Default::default(), models)
        .with_provider_resolver(
            CallConfig {
                selection: Some(ModelSelection {
                    route: "test".into(),
                    model: "fake-1".into(),
                }),
                ..Default::default()
            },
            Arc::new(|selection| {
                if selection.route != "test" {
                    return Err("unknown provider".into());
                }
                Ok(Arc::new(OneAnswer) as Arc<dyn Provider>)
            }),
        ),
    );
    let host = rness_lua::plugin_host::LuaHost::spawn().unwrap();
    host.install_session(
        sessions.clone(),
        Arc::new(rness_engine::subagent::SubagentRuntime::new(
            sessions.clone(),
            3,
        )),
        registry,
        Default::default(),
        tokio::runtime::Handle::current(),
        "fake-1".into(),
    )
    .await
    .unwrap();
    host.load(
        "models",
        include_str!("../../../flavors/default/plugins/models.lua"),
    )
    .await
    .unwrap();
    let id = sessions.create(None).unwrap();
    for (command, expected) in [
        ("/model ", "test/fake-1"),
        ("/profile ", "restricted"),
        ("/model-settings ", "reasoning high"),
    ] {
        let prepared = sessions.prepare_command(&id, command).unwrap().unwrap();
        assert!(prepared
            .complete(&sessions)
            .unwrap()
            .contains(&expected.to_string()));
    }
    for command in [
        "/model-settings temperature 0.4",
        "/model-settings max-output-tokens 2048",
        "/model-settings reasoning high",
    ] {
        let result = sessions
            .send(
                &id,
                UserIntent::Followup,
                vec![ContentPart::Text {
                    text: command.into(),
                }],
            )
            .unwrap();
        assert!(matches!(
            result,
            rness_engine::inbox::Disposition::Command(_)
        ));
    }
    let config = sessions.config(&id).unwrap();
    assert_eq!(config.temperature, Some(0.4));
    assert_eq!(config.max_output_tokens, Some(2048));
    assert_eq!(
        config.reasoning,
        Some(Reasoning::Effort {
            effort: "high".into()
        })
    );
    for command in [
        "/model malformed",
        "/model unknown/model",
        "/model-settings temperature nope",
        "/model-settings max-output-tokens -1",
    ] {
        assert!(sessions
            .send(
                &id,
                UserIntent::Followup,
                vec![ContentPart::Text {
                    text: command.into()
                }]
            )
            .is_err());
        assert_eq!(sessions.config(&id).unwrap(), config);
    }
    sessions
        .send(
            &id,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "/model-settings temperature default".into(),
            }],
        )
        .unwrap();
    let replayed = sessions.replay(&id).unwrap();
    assert_eq!(replayed.context.config.temperature, None);
    assert_eq!(replayed.context.config.max_output_tokens, Some(2048));
    sessions
        .send(
            &id,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "/profile restricted".into(),
            }],
        )
        .unwrap();
    let restricted = sessions.replay(&id).unwrap().context.config;
    assert_eq!(restricted.selection.as_ref().unwrap().model, "restricted");
    assert_eq!(restricted.temperature, None);
    assert_eq!(restricted.max_output_tokens, None);
    assert_eq!(restricted.reasoning, None);
    for field in [
        "temperature",
        "max-output-tokens",
        "reasoning",
        "budget-tokens",
    ] {
        assert!(sessions
            .send(
                &id,
                UserIntent::Followup,
                vec![ContentPart::Text {
                    text: format!("/model-settings {field} 1")
                }]
            )
            .is_err());
    }
    let rness_engine::inbox::Disposition::Command(result) = sessions
        .send(
            &id,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "/model-settings".into(),
            }],
        )
        .unwrap()
    else {
        panic!("expected command")
    };
    assert!(!result.message.contains("Temperature:"));
    assert!(!result.message.contains("Max output tokens:"));
    assert_eq!(
        sessions
            .profile_config("restricted", None)
            .unwrap()
            .temperature,
        None
    );
    assert_eq!(
        sessions
            .config(&sessions.create(None).unwrap())
            .unwrap()
            .selection
            .unwrap()
            .model,
        "fake-1"
    );
    assert!(!replayed
        .history
        .iter()
        .any(|env| matches!(env.event, SessionEvent::TurnStarted { .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn lua_commands_execute_without_model_turn_and_unload_from_service() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(OneAnswer),
        registry.clone(),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let host = rness_lua::plugin_host::LuaHost::spawn().unwrap();
    host.install_session(
        sessions.clone(),
        Arc::new(rness_engine::subagent::SubagentRuntime::new(
            sessions.clone(),
            3,
        )),
        registry,
        Default::default(),
        tokio::runtime::Handle::current(),
        "fake-1".into(),
    )
    .await
    .unwrap();
    host.load("commands", r#"
        rness.commands.register{name='greet', description='Greet', usage='<name>', arguments={'world', 'team'}, run=function(ctx)
            return {message='hello' .. ctx.raw_input, data={session=ctx.session}}
        end}
        rness.commands.register{name='dynamic', complete=function(ctx) return {ctx.session .. ctx.raw_input} end, run=function() end}
        rness.commands.register{name='spin', run=function() while true do end end}
        rness.commands.register{name='broken', run=function() error('command failed') end}
    "#).await.unwrap();
    assert!(sessions
        .commands()
        .help("greet")
        .unwrap()
        .message
        .contains("<name>"));
    assert!(sessions
        .commands()
        .completions()
        .iter()
        .any(|(name, _)| name == "greet world"));
    let id = sessions.create(None).unwrap();
    let before = sessions.store().history(&id).unwrap();
    let completion = sessions
        .prepare_command(&id, "/dynamic query")
        .unwrap()
        .unwrap();
    assert_eq!(
        completion.complete(&sessions).unwrap(),
        vec![format!("{id} query")]
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    host.load("http-command", &format!("rness.commands.register{{name='network', run=function() return rness.http.get({url:?}).body end}}")).await.unwrap();
    let network = tokio::spawn(sessions.send_async(
        id.clone(),
        UserIntent::Followup,
        vec![ContentPart::Text {
            text: "/network".into(),
        }],
    ));
    let (socket, _) = tokio::time::timeout(std::time::Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    sessions.cancel(&id);
    let error = tokio::time::timeout(std::time::Duration::from_secs(3), network)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    drop(socket);
    assert!(host
        .unload_coordinated("http-command", vec![], |_| {})
        .await
        .unwrap());
    #[cfg(unix)]
    {
        let marker = dir.path().join("process-ready");
        host.load("process-command", &format!(r#"rness.commands.register{{name='external', run=function()
            return rness.process.run{{program='/bin/sh', args={{'-c', 'echo ready > "$1"; sleep 100 & wait', 'sh', {:?}}}}}.stdout
        end}}"#, marker.to_str().unwrap())).await.unwrap();
        let external = tokio::spawn(sessions.send_async(
            id.clone(),
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "/external".into(),
            }],
        ));
        let ready = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !marker.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        sessions.cancel(&id);
        let error = tokio::time::timeout(std::time::Duration::from_secs(3), external)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        ready.unwrap();
        assert!(error.to_string().contains("cancelled"));
        assert!(sessions.try_extension_maintenance().is_ok());
        assert!(host
            .unload_coordinated("process-command", vec![], |_| {})
            .await
            .unwrap());
    }
    let result = sessions
        .send(
            &id,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "/greet  world".into(),
            }],
        )
        .unwrap();
    let rness_engine::inbox::Disposition::Command(result) = result else {
        panic!("expected command result")
    };
    assert_eq!(result.message, "hello  world");
    assert_eq!(result.data["session"], id);
    assert!(sessions
        .send(
            &id,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "/broken".into()
            }]
        )
        .is_err());
    assert_eq!(sessions.store().history(&id).unwrap(), before);

    // Pause between admission and execution: unload must not invalidate the
    // captured handler or reinterpret the original input as a model prompt.
    let prepared = sessions
        .prepare_command(&id, "/greet reserved")
        .unwrap()
        .unwrap();
    let unload = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        host.unload_coordinated("commands", vec![], |_| {}),
    )
    .await
    .unwrap();
    assert!(unload.unwrap_err().contains("busy"));
    assert!(matches!(
        prepared.execute(&sessions).unwrap(),
        rness_engine::inbox::Disposition::Command(_)
    ));
    assert_eq!(sessions.store().history(&id).unwrap(), before);

    let cancelled = sessions.send_async(
        id.clone(),
        UserIntent::Followup,
        vec![ContentPart::Text {
            text: "/spin".into(),
        }],
    );
    sessions.cancel(&id);
    assert!(cancelled
        .await
        .unwrap_err()
        .to_string()
        .contains("cancelled"));
    assert!(!sessions.command_running(&id));

    // A native handler leaves the actor free to process unload while the
    // command is actually executing, not merely waiting for admission.
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    sessions
        .commands()
        .register(Arc::new(BlockingCommand {
            entered: std::sync::Mutex::new(Some(entered_tx)),
            release: std::sync::Mutex::new(release_rx),
        }))
        .unwrap();
    let blocked = tokio::spawn(sessions.send_async(
        id.clone(),
        UserIntent::Followup,
        vec![ContentPart::Text {
            text: "/blocked".into(),
        }],
    ));
    tokio::time::timeout(std::time::Duration::from_secs(3), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let unload = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        host.unload_coordinated("commands", vec![], |_| {}),
    )
    .await;
    release_tx.send(()).unwrap();
    assert!(unload.unwrap().unwrap_err().contains("busy"));
    blocked.await.unwrap().unwrap();
    assert_eq!(sessions.store().history(&id).unwrap(), before);
    let running = {
        let sessions = sessions.clone();
        let id = id.clone();
        tokio::spawn(async move {
            sessions
                .send_async(
                    id,
                    UserIntent::Followup,
                    vec![ContentPart::Text {
                        text: "/spin".into(),
                    }],
                )
                .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if sessions.command_running(&id) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(sessions
        .send(
            &id,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "model prompt".into()
            }]
        )
        .is_err());
    assert!(sessions
        .send(
            &id,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "/greet".into()
            }]
        )
        .is_err());
    sessions.cancel(&id);
    let error = tokio::time::timeout(std::time::Duration::from_secs(3), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(sessions.try_extension_maintenance().is_ok());
    assert!(host
        .unload_coordinated("commands", vec![], |_| {})
        .await
        .unwrap());
    assert!(sessions.commands().resolve("/greet").is_none());
    assert!(sessions.commands().resolve("/agent").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn lua_drives_a_session_through_the_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(OneAnswer),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));

    let subagents = Arc::new(rness_engine::subagent::SubagentRuntime::new(
        Arc::clone(&sessions),
        3,
    ));
    subagents.register(Arc::new(rness_engine::subagent::SpawnProvider));
    subagents.register(Arc::new(rness_engine::subagent::ForkProvider));
    let host = rness_lua::plugin_host::LuaHost::spawn().unwrap();
    host.install_session(
        Arc::clone(&sessions),
        subagents,
        Arc::new(ToolRegistry::default()),
        Default::default(),
        tokio::runtime::Handle::current(),
        "test/model".into(),
    )
    .await
    .unwrap();

    // Create from Rust; drive from Lua.
    let id = sessions.create(None).unwrap();
    host.load(
        "driver.lua",
        &format!(
            r#"
            local id = "{id}"
            assert(rness.session.phase(id) == "idle")
            assert(rness.session.send(id, "hola") == "started")
            rness.tool.register{{
              name = "probe",
              run = function()
                local t = rness.session.transcript(id)
                local last = t[#t]
                return rness.session.phase(id) .. "|" .. #t .. "|" .. last.role .. "|" .. last.text
              end,
            }}
            "#
        ),
    )
    .await
    .unwrap();

    sessions.join(&id).await;

    let out = host
        .call_tool("probe", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out, "idle|2|assistant|hola desde el modelo");

    // Fork from Lua: a real child session appears in the store.
    host.load(
        "fork.lua",
        &format!(r#"child = rness.session.fork("{id}")"#),
    )
    .await
    .unwrap();
    assert_eq!(sessions.list().unwrap().len(), 2);
}

/// Step 1 parks until released (so Lua can steer mid-turn), then calls an
/// unknown tool to force a step boundary; step 2 records whether the steer
/// text reached the model.
struct SteerProbe {
    step: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    saw_steer: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Provider for SteerProbe {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, request: StepRequest<'_>, _cancel: &CancellationToken) -> StepOutcome {
        let n = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (content, stop) = if n == 0 {
            self.entered.notify_one();
            self.release.notified().await;
            (
                vec![ContentPart::ToolUse {
                    call: "c1".into(),
                    name: "absent".into(),
                    args: serde_json::json!({}),
                }],
                StopReason::ToolUse,
            )
        } else {
            let seen = request.context.turns.iter().any(|t| {
                matches!(t, rness_engine::session::projection::ModelTurn::User { content }
                    if content.iter().any(|p| matches!(p, ContentPart::Text { text } if text == "steer me")))
            });
            self.saw_steer
                .store(seen, std::sync::atomic::Ordering::SeqCst);
            (
                vec![ContentPart::Text {
                    text: "done".into(),
                }],
                StopReason::EndTurn,
            )
        };
        StepOutcome::Committed(AssistantMessage {
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
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lua_steer_joins_the_running_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(SteerProbe {
        step: Default::default(),
        entered: Default::default(),
        release: Default::default(),
        saw_steer: Default::default(),
    });
    let store = SessionStore::new(dir.path());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        provider.clone(),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let subagents = Arc::new(rness_engine::subagent::SubagentRuntime::new(
        Arc::clone(&sessions),
        3,
    ));
    let host = rness_lua::plugin_host::LuaHost::spawn().unwrap();
    host.install_session(
        Arc::clone(&sessions),
        subagents,
        Arc::new(ToolRegistry::default()),
        Default::default(),
        tokio::runtime::Handle::current(),
        "test/model".into(),
    )
    .await
    .unwrap();

    let id = sessions.create(None).unwrap();
    // Idle: steer behaves like a prompt and starts the turn.
    host.load(
        "start.lua",
        &format!(r#"assert(rness.session.steer("{id}", "go") == "started")"#),
    )
    .await
    .unwrap();
    provider.entered.notified().await;
    // Running: steer is queued for the next step boundary of this same turn.
    host.load(
        "steer.lua",
        &format!(r#"assert(rness.session.steer("{id}", "steer me") == "queued")"#),
    )
    .await
    .unwrap();
    provider.release.notify_one();
    sessions.join(&id).await;

    assert!(
        provider.saw_steer.load(std::sync::atomic::Ordering::SeqCst),
        "step 2 must see the steer"
    );
    let events = store.read_session(&id).unwrap();
    let turns = events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::TurnStarted { .. }))
        .count();
    assert_eq!(turns, 1, "steer must not start a separate turn");
    assert!(events.iter().any(|e| matches!(&e.event,
        SessionEvent::UserMessage(m) if m.intent == UserIntent::Steer)));
}

#[tokio::test(flavor = "multi_thread")]
async fn lua_delegates_to_a_subagent() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(OneAnswer),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));

    let subagents = Arc::new(
        rness_engine::subagent::SubagentRuntime::new(Arc::clone(&sessions), 3)
            .with_allow_generic(true),
    );
    subagents.register(Arc::new(rness_engine::subagent::SpawnProvider));
    subagents.register(Arc::new(rness_engine::subagent::ForkProvider));
    let host = rness_lua::plugin_host::LuaHost::spawn().unwrap();
    host.install_session(
        Arc::clone(&sessions),
        subagents,
        Arc::new(ToolRegistry::default()),
        Default::default(),
        tokio::runtime::Handle::current(),
        "test/model".into(),
    )
    .await
    .unwrap();

    let id = sessions.create(None).unwrap();
    host.load(
        "delegator.lua",
        &format!(
            r#"
            local names = rness.subagents.providers()
            assert(names[1] == "fork" and names[2] == "spawn", "providers listed")
            local run = rness.subagents.start("spawn", {{ parent = "{id}", prompt = "haz algo" }})
            assert(run.stop == "completed", "child completed")
            assert(run.output == "hola desde el modelo", "child answered")
            local d = rness.subagents.delegation(run.session)
            assert(d.parent == "{id}" and d.depth == 1, "lineage stamped")
            assert(rness.subagents.delegation("{id}") == nil, "root undelegated")

            -- continuable lifecycle from Lua
            local kid = rness.subagents.start_continuable("spawn", {{ parent = "{id}", prompt = "quedate" }})
            local kd = rness.subagents.delegation(kid)
            assert(kd.mode == "continuable", "continuable mode stamped")
            local kids = rness.subagents.children("{id}")
            assert(#kids == 1 and kids[1].session == kid, "child listed")
            assert(kids[1].depth == 1, "depth reported")
            rness.subagents.send_message("{id}", kid, "sigue")
            rness.subagents.interrupt("{id}", kid)
            local ok = pcall(rness.subagents.send_message, kid .. "x", kid, "no")
            assert(not ok, "stranger refused")
            "#
        ),
    )
    .await
    .unwrap();
    // Parent + one-shot child + continuable child, all ordinary sessions.
    assert_eq!(sessions.list().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_bridge_survives_hot_reload() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(OneAnswer),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));

    let subagents = Arc::new(rness_engine::subagent::SubagentRuntime::new(
        Arc::clone(&sessions),
        3,
    ));
    subagents.register(Arc::new(rness_engine::subagent::SpawnProvider));
    subagents.register(Arc::new(rness_engine::subagent::ForkProvider));
    let host = rness_lua::plugin_host::LuaHost::spawn().unwrap();
    host.install_session(
        Arc::clone(&sessions),
        subagents,
        Arc::new(ToolRegistry::default()),
        Default::default(),
        tokio::runtime::Handle::current(),
        "test/model".into(),
    )
    .await
    .unwrap();

    // Swap the VM — the fresh one must get rness.session re-injected,
    // even usable at load time.
    let id = sessions.create(None).unwrap();
    host.reload(vec![rness_lua::loader::PluginSource {
        dependencies: vec![],
        name: "reloaded.lua".into(),
        source: format!(r#"assert(rness.session.phase("{id}") == "idle")"#),
    }])
    .await
    .unwrap();
}
