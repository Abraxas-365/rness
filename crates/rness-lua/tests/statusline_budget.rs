//! Regression: the default statusline must never be disabled by slow
//! engine lookups.
//!
//! The first `rness.session.usage` of a long session folds its whole log and
//! the first `rness.subagents.list` reads every session header in the
//! store; both measured 300-650 ms on real stores. Native calls are not
//! interrupted by the budget hook, so when they ran inside the statusline
//! render (250 ms budget) three slow renders in a row disabled the provider
//! ("Lua statusline provider exceeded its 250 ms budget 3 times in a row").
//! The plugin now runs them in background tasks and renders from caches.
use async_trait::async_trait;
use rness_engine::{
    service::SessionService,
    session::branch::SessionStore,
    tools::ToolRegistry,
    turn::{
        provider::{Provider, StepOutcome, StepRequest},
        TurnConfig,
    },
};
use rness_kernel::presentation::TextProvider;
use rness_kernel::EventBus;
use rness_lua::plugin_host::LuaHost;
use rness_protocol::events::*;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const STATUSLINE: &str = include_str!("../../../flavors/default/plugins/statusline.lua");

/// Lookups that block natively for longer than the render budget, like a
/// cold usage fold or store scan. `rness.process.run` is a native call the
/// budget hook cannot interrupt, exactly like the engine lookups.
const SLOW_STUBS: &str = r#"
    local function block()
      rness.process.run { program = "sleep", args = { "0.3" } }
    end
    usage_calls, list_calls = 0, 0
    rness.session = {
      usage = function()
        usage_calls = usage_calls + 1
        block()
        return { input = 42000, cache_read = 1000 }
      end,
    }
    rness.subagents = {
      list = function()
        list_calls = list_calls + 1
        block()
        return { { session = "child", running = true } }
      end,
    }
"#;

async fn render(host: &LuaHost, session: &str) -> Option<String> {
    host.status(json!({ "session": session, "model": "test" }))
        .await
        .map(|v| v.to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_lookups_never_disable_the_statusline() {
    let host = LuaHost::spawn().unwrap();
    host.load("stubs", SLOW_STUBS).await.unwrap();
    host.load("statusline", STATUSLINE).await.unwrap();
    // Several sessions (resume, switch): each first render used to cost a
    // slow render, and three in a row disabled the provider.
    for session in ["one", "two", "three"] {
        let text = render(&host, session).await;
        assert!(text.is_some(), "statusline disabled while showing {session}");
    }
    // The lookups really ran (in tasks) and their results are shown.
    let text = render(&host, "three").await.expect("statusline disabled");
    assert!(text.contains("42.0k last input (+1.0k cached)"), "{text}");
    assert!(text.contains("1 bg agent"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn render_does_not_wait_for_slow_lookups() {
    let host = LuaHost::spawn().unwrap();
    host.load("stubs", SLOW_STUBS).await.unwrap();
    host.load("statusline", STATUSLINE).await.unwrap();
    // A first render of a session queues the lookups but must return at
    // once: the queued work runs after it, not inside it.
    let started = Instant::now();
    let text = render(&host, "fresh").await.expect("statusline disabled");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "first render blocked on lookups: {elapsed:?}"
    );
    assert!(text.contains("idle"), "{text}");
    // One poll later the cached values are shown.
    let text = render(&host, "fresh").await.unwrap();
    assert!(text.contains("42.0k last input"), "{text}");
    assert!(text.contains("1 bg agent"), "{text}");
}

#[tokio::test]
async fn lookups_are_not_queued_twice_while_one_is_pending() {
    let host = LuaHost::spawn().unwrap();
    host.load("stubs", SLOW_STUBS).await.unwrap();
    host.load("statusline", STATUSLINE).await.unwrap();
    // Renders and refresh hooks while the first lookups are still queued
    // collapse into one task per (kind, session): no backlog builds up
    // behind a slow store.
    host.fire_hook("turn_end", json!({ "session": "s" }));
    host.fire_hook("frame", json!({ "type": "step_committed", "session": "s" }));
    render(&host, "s").await.unwrap();
    render(&host, "s").await.unwrap();
    host.load(
        "assert",
        "assert(usage_calls == 1, 'usage calls: ' .. usage_calls)
         assert(list_calls <= 2, 'list calls: ' .. list_calls)",
    )
    .await
    .unwrap();
}

struct Unused;
#[async_trait]
impl Provider for Unused {
    fn model(&self) -> &str {
        "test"
    }
    async fn step(&self, _: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
        panic!("no model calls expected")
    }
}

fn reply(input: u64) -> SessionEvent {
    SessionEvent::AssistantMessage(AssistantMessage {
        model: "fake".into(),
        content: vec![],
        stop: StopReason::EndTurn,
        usage: Usage {
            input_tokens: input,
            output_tokens: 1,
            cache_read_tokens: 500,
            ..Default::default()
        },
        estimated_input: 0,
        chunks: vec![],
    })
}

/// The default statusline against a real engine: the real
/// `rness.session.usage` / `rness.subagents.list` work from background
/// tasks, and renders stay enabled and show their results.
#[tokio::test(flavor = "multi_thread")]
async fn default_statusline_on_a_real_engine_stays_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(Some("/w".into())).unwrap();
    let id = log.session().clone();
    for i in 0..200u64 {
        log.append(&SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text: "hi".into() }],
            source: None,
        }))
        .unwrap();
        log.append(&reply(5_800 + i)).unwrap();
    }
    drop(log);
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        store,
        Arc::new(Unused),
        registry.clone(),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let host = LuaHost::spawn().unwrap();
    host.install_session(
        sessions.clone(),
        Arc::new(rness_engine::subagent::SubagentRuntime::new(
            sessions.clone(),
            3,
        )),
        registry,
        Default::default(),
        tokio::runtime::Handle::current(),
        "test".into(),
    )
    .await
    .unwrap();
    host.load("statusline", STATUSLINE).await.unwrap();
    let mut text = String::new();
    for _ in 0..50 {
        text = render(&host, &id).await.expect("statusline disabled");
        if text.contains("last input") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(text.contains("idle"), "{text}");
    assert!(text.contains("6.0k last input (+0.5k cached)"), "{text}");
}
