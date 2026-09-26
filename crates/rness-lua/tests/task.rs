//! rness.task: background coroutines that await async APIs on a live host.
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::EventBus;
use rness_lua::plugin_host::LuaHost;
use rness_protocol::events::*;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Title requests wait on `gate`; turns answer at once.
struct GatedTitle {
    gate: Semaphore,
}

#[async_trait]
impl Provider for GatedTitle {
    fn model(&self) -> &str {
        "test"
    }
    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        let text = if request.system.starts_with("Create a concise title") {
            tokio::select! {
                _ = cancel.cancelled() => return StepOutcome::Cancelled { partial: vec![] },
                permit = self.gate.acquire() => permit.unwrap().forget(),
            }
            "Parser refactor"
        } else {
            "done"
        };
        StepOutcome::Committed(AssistantMessage {
            model: "test".into(),
            content: vec![ContentPart::Text { text: text.into() }],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
            estimated_input: 0,
            chunks: vec![],
        })
    }
}

async fn host() -> (LuaHost, Arc<SessionService>, Arc<GatedTitle>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(GatedTitle { gate: Semaphore::new(0) });
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        provider.clone(),
        registry.clone(),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let host = LuaHost::spawn().unwrap();
    host.install_session(
        sessions.clone(),
        Arc::new(rness_engine::subagent::SubagentRuntime::new(sessions.clone(), 3)),
        registry,
        Default::default(),
        tokio::runtime::Handle::current(),
        "test".into(),
    )
    .await
    .unwrap();
    (host, sessions, provider, dir)
}

async fn eventually(mut cond: impl FnMut() -> bool) {
    for _ in 0..300 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

async fn session_with_prompt(sessions: &SessionService) -> String {
    let sid = sessions.create(None).unwrap();
    sessions
        .send(&sid, UserIntent::Followup, vec![ContentPart::Text { text: "refactor the parser".into() }])
        .unwrap();
    sessions.join(&sid).await;
    sid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_awaits_generate_title_from_a_hook_and_keeps_vm_free() {
    let (host, sessions, provider, _dir) = host().await;
    let sid = session_with_prompt(&sessions).await;
    host.load(
        "titler",
        r#"
        rness.hook.on("go", function(ev)
          rness.task.spawn(function(session)
            local title = rness.session.generate_title(session, { max_bytes = 6 })
            rness.session.offer_title(session, title, "model", 80)
          end, ev.session)
        end)
        "#,
    )
    .await
    .unwrap();
    host.fire_hook("go", serde_json::json!({ "session": sid }));
    // The task is parked on the gated model call; the VM keeps serving.
    tokio::time::sleep(Duration::from_millis(50)).await;
    host.load("other", "x = 1").await.unwrap();
    assert_eq!(sessions.title(&sid).unwrap(), None);
    provider.gate.add_permits(1);
    eventually(|| sessions.title(&sid).unwrap().is_some()).await;
    assert_eq!(sessions.title(&sid).unwrap().as_deref(), Some("Parser"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unload_and_cancel_stop_parked_tasks_and_errors_are_contained() {
    let (host, sessions, provider, _dir) = host().await;
    let sid = session_with_prompt(&sessions).await;
    host.load(
        "titler",
        r#"
        rness.hook.on("go", function(ev)
          rness.task.spawn(function()
            local title = rness.session.generate_title(ev.session)
            rness.session.offer_title(ev.session, title, "model")
          end)
        end)
        rness.hook.on("cancel", function(ev)
          local id = rness.task.spawn(function()
            local title = rness.session.generate_title(ev.session)
            rness.session.offer_title(ev.session, "cancelled " .. title, "model")
          end)
          assert(rness.task.cancel(id) == true)
          assert(rness.task.cancel(id) == false)
        end)
        rness.hook.on("boom", function() rness.task.spawn(function() error("kaput") end) end)
        rness.hook.on("bad_yield", function() rness.task.spawn(function() coroutine.yield(1) end) end)
        "#,
    )
    .await
    .unwrap();
    host.fire_hook("boom", serde_json::json!({}));
    host.fire_hook("bad_yield", serde_json::json!({}));
    host.fire_hook("cancel", serde_json::json!({ "session": sid }));
    host.fire_hook("go", serde_json::json!({ "session": sid }));
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Unloading the owner cancels the parked task: releasing the model
    // afterwards commits nothing.
    assert!(host.unload_coordinated("titler", vec![], |_| {}).await.unwrap());
    // One permit: the cancelled tasks never consume it, so "again" does.
    provider.gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(sessions.title(&sid).unwrap(), None);
    // The VM survived every failure and still runs new tasks.
    host.load(
        "again",
        r#"
        rness.hook.on("go", function(ev)
          rness.task.spawn(function()
            rness.session.offer_title(ev.session, rness.session.generate_title(ev.session), "model")
          end)
        end)
        "#,
    )
    .await
    .unwrap();
    host.fire_hook("go", serde_json::json!({ "session": sid }));
    eventually(|| sessions.title(&sid).unwrap().is_some()).await;
    assert_eq!(sessions.title(&sid).unwrap().as_deref(), Some("Parser refactor"));

    // A hot reload cancels the old generation's parked tasks too.
    sessions.set_title(&sid, "Before reload".into(), TitleSource::Model).unwrap();
    host.fire_hook("go", serde_json::json!({ "session": sid }));
    tokio::time::sleep(Duration::from_millis(50)).await;
    host.reload(vec![]).await.unwrap();
    provider.gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(sessions.title(&sid).unwrap().as_deref(), Some("Before reload"));
}

/// The real plugin on a real host: root sessions carry `parent` = JSON null
/// (not nil), which must still be titled; delegated sessions are skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_title_plugin_titles_root_sessions_through_a_live_host() {
    let (host, sessions, provider, _dir) = host().await;
    let sid = session_with_prompt(&sessions).await;
    let child = sessions
        .create_delegated(
            None,
            rness_protocol::branch::Delegation {
                parent: sid.clone(),
                call: None,
                depth: 1,
                mode: Default::default(),
            },
        )
        .unwrap();
    let plugin = format!(
        "local setup = (function() {} end)()\nsetup({{}})",
        include_str!("../../../flavors/default/plugins/session-title.lua")
    );
    host.load("session-title", &plugin).await.unwrap();
    host.fire_hook(
        "prompt",
        serde_json::json!({ "session": child, "index": 1, "text": "child work" }),
    );
    host.fire_hook(
        "prompt",
        serde_json::json!({ "session": sid, "index": 1, "text": "refactor the parser" }),
    );
    eventually(|| sessions.title(&sid).unwrap().as_deref() == Some("refactor the parser")).await;
    provider.gate.add_permits(1);
    eventually(|| sessions.title(&sid).unwrap().as_deref() == Some("Parser refactor")).await;
    assert_eq!(sessions.title(&child).unwrap(), None);
}
