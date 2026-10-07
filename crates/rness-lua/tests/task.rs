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

/// Title requests wait on `gate`; turns wait on `turn_gate` once
/// `gate_turns` is set, else answer at once.
struct GatedTitle {
    gate: Semaphore,
    turn_gate: Semaphore,
    gate_turns: std::sync::atomic::AtomicBool,
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
            if self.gate_turns.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::select! {
                    _ = cancel.cancelled() => return StepOutcome::Cancelled { partial: vec![] },
                    permit = self.turn_gate.acquire() => permit.unwrap().forget(),
                }
            }
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

async fn host() -> (
    LuaHost,
    Arc<SessionService>,
    Arc<GatedTitle>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(GatedTitle {
        gate: Semaphore::new(0),
        turn_gate: Semaphore::new(0),
        gate_turns: Default::default(),
    });
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
    // The async call every test task awaits (the gated provider recognises
    // the title system prompt). A global so every plugin can use it.
    host.load(
        "helpers",
        r#"function title_of(session)
          return rness.llm.complete { session = session, system = "Create a concise title", prompt = "x" }
        end"#,
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
        .send(
            &sid,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "refactor the parser".into(),
            }],
        )
        .unwrap();
    sessions.join(&sid).await;
    sid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_awaits_llm_complete_from_a_hook_and_keeps_vm_free() {
    let (host, sessions, provider, _dir) = host().await;
    let sid = session_with_prompt(&sessions).await;
    host.load(
        "titler",
        r#"
        rness.hook.on("go", function(ev)
          rness.task.spawn(function(session)
            local title = title_of(session)
            rness.session.offer_title(session, title, "model", 6)
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
            local title = title_of(ev.session)
            rness.session.offer_title(ev.session, title, "model")
          end)
        end)
        rness.hook.on("cancel", function(ev)
          local id = rness.task.spawn(function()
            local title = title_of(ev.session)
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
    assert!(host
        .unload_coordinated("titler", vec![], |_| {})
        .await
        .unwrap());
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
            rness.session.offer_title(ev.session, title_of(ev.session), "model")
          end)
        end)
        "#,
    )
    .await
    .unwrap();
    host.fire_hook("go", serde_json::json!({ "session": sid }));
    eventually(|| sessions.title(&sid).unwrap().is_some()).await;
    assert_eq!(
        sessions.title(&sid).unwrap().as_deref(),
        Some("Parser refactor")
    );

    // A hot reload cancels the old generation's parked tasks too.
    sessions
        .set_title(&sid, "Before reload".into(), TitleSource::Model, 80)
        .unwrap();
    host.fire_hook("go", serde_json::json!({ "session": sid }));
    tokio::time::sleep(Duration::from_millis(50)).await;
    host.reload(vec![]).await.unwrap();
    provider.gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        sessions.title(&sid).unwrap().as_deref(),
        Some("Before reload")
    );
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
        include_str!("../../../flavors/default/plugins/title.lua")
    );
    host.load("title", &plugin).await.unwrap();
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

/// Run `/title …` the way the TUI does: prepare, then execute off-thread.
async fn run_title(sessions: &Arc<SessionService>, sid: &str, input: &str) -> String {
    let command = sessions
        .prepare_command(&sid.to_string(), input)
        .unwrap()
        .expect("title command registered");
    let sessions = sessions.clone();
    match tokio::task::spawn_blocking(move || command.execute(&sessions))
        .await
        .unwrap()
        .unwrap()
    {
        rness_engine::inbox::Disposition::Command(result) => result.message,
        other => panic!("unexpected disposition {other:?}"),
    }
}

/// `/title` works mid-turn (allow_busy) and `/title auto` returns at once,
/// never holding the session; its result arrives as a notice frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn title_command_runs_during_a_turn_and_auto_reports_by_notice() {
    let (host, sessions, provider, _dir) = host().await;
    let sid = session_with_prompt(&sessions).await;
    let notices: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = notices.clone();
    let _sub = sessions
        .bus()
        .on::<rness_engine::service::FrameEv>(move |frame| {
            if let rness_protocol::frames::Frame::Notice { text, .. } = frame {
                seen.lock().unwrap().push(text.clone());
            }
        });
    let plugin = format!(
        "local setup = (function() {} end)()\nsetup({{ auto = 'off', fallback = false }})",
        include_str!("../../../flavors/default/plugins/title.lua")
    );
    host.load("title", &plugin).await.unwrap();

    // A turn is running.
    provider
        .gate_turns
        .store(true, std::sync::atomic::Ordering::SeqCst);
    sessions
        .send(
            &sid,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "keep going".into(),
            }],
        )
        .unwrap();
    assert_eq!(sessions.phase(&sid), rness_engine::inbox::Phase::Running);

    assert_eq!(
        run_title(&sessions, &sid, "/title Mid turn").await,
        "Title set: Mid turn"
    );
    assert_eq!(sessions.title(&sid).unwrap().as_deref(), Some("Mid turn"));

    // /title auto returns before the model answers; the session stays usable.
    assert_eq!(
        run_title(&sessions, &sid, "/title auto").await,
        "Generating title…"
    );
    assert!(!sessions.command_running(&sid));
    assert_eq!(
        run_title(&sessions, &sid, "/title").await,
        "Mid turn (pinned)"
    );
    provider.gate.add_permits(1);
    eventually(|| {
        notices
            .lock()
            .unwrap()
            .iter()
            .any(|n| n == "Title: Parser refactor")
    })
    .await;
    assert_eq!(
        sessions.title(&sid).unwrap().as_deref(),
        Some("Parser refactor")
    );

    // Renaming supersedes an in-flight /title auto.
    run_title(&sessions, &sid, "/title auto").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        run_title(&sessions, &sid, "/title Final").await,
        "Title set: Final"
    );
    provider.gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(sessions.title(&sid).unwrap().as_deref(), Some("Final"));
    assert_eq!(
        notices.lock().unwrap().len(),
        1,
        "superseded request reports nothing"
    );

    provider.turn_gate.add_permits(1);
    sessions.join(&sid).await;
    assert!(
        sessions.notify(&sid, " \u{1b} ").is_err(),
        "empty notice after stripping controls"
    );
    drop(host);
}
