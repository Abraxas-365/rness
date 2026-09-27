//! `rness.context.ensure`: plugin-maintained context blocks the engine
//! keeps visible to the model (first step of each turn, after compaction).

use rness_engine::turn::hooks::{ContextBlock, LoopEvent, LoopHooks};
use rness_lua::plugin_host::LuaHost;
use tokio_util::sync::CancellationToken;

fn ev() -> LoopEvent {
    LoopEvent {
        session: "s1".into(),
        turn: 1,
        step: 1,
    }
}

async fn blocks(host: &LuaHost) -> Vec<ContextBlock> {
    host.context(&ev(), &CancellationToken::new())
        .await
        .unwrap()
}

#[tokio::test]
async fn no_renderers_means_no_blocks() {
    let host = LuaHost::spawn().unwrap();
    assert!(blocks(&host).await.is_empty());
}

#[tokio::test]
async fn renderers_return_blocks_in_registration_order() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "memory",
        r#"
        rness.context.ensure({ name = "memory", render = function(ev)
          return "index for " .. ev.session .. " turn " .. ev.turn, "v7"
        end })
        rness.context.ensure({ name = "notes", render = function() return "notes" end })
        "#,
    )
    .await
    .unwrap();
    let got = blocks(&host).await;
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].name, "memory");
    assert_eq!(got[0].text, "index for s1 turn 1");
    assert_eq!(got[0].identity, "v7");
    assert_eq!(got[1].name, "notes");
    // Default identity: a stable content hash, changing with the text.
    assert_eq!(
        got[1].identity,
        rness_engine::instructions::fingerprint("notes")
    );
}

#[tokio::test]
async fn empty_nil_and_failing_renderers_are_skipped() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "p",
        r#"
        rness.context.ensure({ name = "nil", render = function() return nil end })
        rness.context.ensure({ name = "empty", render = function() return "" end })
        rness.context.ensure({ name = "boom", render = function() error("kaput") end })
        rness.context.ensure({ name = "bad", render = function() return 42 end })
        rness.context.ensure({ name = "ok", render = function() return "fine" end })
        "#,
    )
    .await
    .unwrap();
    let got = blocks(&host).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].name, "ok");
}

#[tokio::test]
async fn invalid_specs_fail_at_registration() {
    for bad in [
        r#"rness.context.ensure({ render = function() end })"#,
        r#"rness.context.ensure({ name = "", render = function() end })"#,
        r#"rness.context.ensure({ name = "has space", render = function() end })"#,
        r#"rness.context.ensure({ name = "x" })"#,
    ] {
        let host = LuaHost::spawn().unwrap();
        let err = host.load("p", bad).await.unwrap_err();
        assert!(err.contains("rness.context.ensure"), "{bad}: {err}");
    }
}

#[tokio::test]
async fn duplicate_names_keep_the_first_registration() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "p",
        r#"
        rness.context.ensure({ name = "memory", render = function() return "first" end })
        rness.context.ensure({ name = "memory", render = function() return "second" end })
        rness.context.ensure({ name = "quiet", render = function() return nil end })
        rness.context.ensure({ name = "quiet", render = function() return "shadowed" end })
        "#,
    )
    .await
    .unwrap();
    let got = blocks(&host).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].text, "first");
}

#[tokio::test]
async fn context_event_is_reserved() {
    let host = LuaHost::spawn().unwrap();
    let err = host
        .load(
            "p",
            r#"rness.hook.on("context", function(ev, next) return {} end)"#,
        )
        .await
        .unwrap_err();
    assert!(err.contains("reserved"), "{err}");
    let err = host
        .load("q", r#"rness.events.emit("context", {})"#)
        .await
        .unwrap_err();
    assert!(err.contains("reserved"), "{err}");
}

#[tokio::test]
async fn cancel_interrupts_the_whole_chain() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "p",
        r#"
        rness.context.ensure({ name = "spin", render = function() while true do end end })
        rness.context.ensure({ name = "after", render = function() calls = (calls or 0) + 1 return "x" end })
        "#,
    )
    .await
    .unwrap();
    let cancel = CancellationToken::new();
    let trip = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        trip.cancel();
    });
    assert!(host.context(&ev(), &cancel).await.is_err());
    // The interrupt was not swallowed as a render error: the next
    // renderer in the chain never ran, and the VM is free again.
    host.load(
        "check",
        "assert(calls == nil, 'chain continued after cancel')",
    )
    .await
    .unwrap();
}

/// End to end: a Lua block reaches the model through the real service,
/// with the session workspace in `ev`, and is not repeated while visible.
#[tokio::test(flavor = "multi_thread")]
async fn lua_context_reaches_the_model_once_while_visible() {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use rness_engine::service::SessionService;
    use rness_engine::session::branch::SessionStore;
    use rness_engine::tools::ToolRegistry;
    use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
    use rness_engine::turn::TurnConfig;
    use rness_protocol::events::*;

    /// Records the user-visible text of every request.
    struct Recorder(Mutex<Vec<String>>);
    #[async_trait]
    impl Provider for Recorder {
        fn model(&self) -> &str {
            "fake-1"
        }
        async fn step(&self, request: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
            let texts: Vec<String> = request
                .context
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
            self.0.lock().unwrap().push(texts.join("|"));
            StepOutcome::Committed(AssistantMessage {
                model: "fake-1".into(),
                content: vec![ContentPart::Text { text: "ok".into() }],
                stop: StopReason::EndTurn,
                usage: Usage::default(),
                estimated_input: 0,
                chunks: vec![],
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let provider = Arc::new(Recorder(Mutex::default()));
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        provider.clone(),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(rness_kernel::EventBus::default()),
    ));
    let subagents = Arc::new(rness_engine::subagent::SubagentRuntime::new(
        Arc::clone(&sessions),
        3,
    ));
    let host = LuaHost::spawn().unwrap();
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
    sessions.set_loop_hooks(Some(Arc::new(host.clone())));
    host.load(
        "memory",
        r#"rness.context.ensure({ name = "memory", render = function(ev)
             return "MEMORY for " .. tostring(ev.workspace)
           end })"#,
    )
    .await
    .unwrap();

    let workspace = ws
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let sid = sessions.create(Some(workspace.clone())).unwrap();
    for prompt in ["m1", "m2"] {
        sessions
            .send(
                &sid,
                UserIntent::Followup,
                vec![ContentPart::Text {
                    text: prompt.into(),
                }],
            )
            .unwrap();
        sessions.join(&sid).await;
    }

    let seen = provider.0.lock().unwrap().clone();
    let expected = format!("m1|MEMORY for {workspace}");
    assert_eq!(seen[0], expected);
    // Recorder keeps user turns only: no second MEMORY block before m2.
    assert_eq!(
        seen[1],
        format!("{expected}|m2"),
        "block must not repeat while visible"
    );
    let sourced = sessions
        .store()
        .history(&sid)
        .unwrap()
        .iter()
        .filter(|e| {
            matches!(&e.event, SessionEvent::UserMessage(UserMessage {
            source: Some(MessageSource::Context { name, .. }), ..
        }) if name == "memory")
        })
        .count();
    assert_eq!(sourced, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn disposer_and_unload_remove_the_renderer() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "a",
        r#"off = rness.context.ensure({ name = "a", render = function() return "A" end })"#,
    )
    .await
    .unwrap();
    host.load(
        "b",
        r#"rness.context.ensure({ name = "b", render = function() return "B" end })"#,
    )
    .await
    .unwrap();
    assert_eq!(blocks(&host).await.len(), 2);

    host.load("off", "off()").await.unwrap();
    let got = blocks(&host).await;
    assert_eq!(
        got.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        ["b"]
    );

    assert!(host.unload("b").await.unwrap());
    assert!(blocks(&host).await.is_empty());
    assert!(!host.has_hook("context"));
}
