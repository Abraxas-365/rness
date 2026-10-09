//! WP-3 runaway Lua: what happens to the single VM thread when plugin code
//! never returns, per entry point.
//!
//! Every test asserts the DESIRED behaviour (the VM recovers). A failing test
//! means a confirmed bug (see ~/.rness/plans/rness/e2e-findings/wp-3.md).
//! All tests are `#[ignore]`: a hung VM thread spins one core until the test
//! binary exits. Run them with a hard timeout, one at a time or all together:
//!
//!   timeout 300 cargo test -p rness-lua --test runaway_e2e -- --ignored --test-threads 4
//!
//! The probe is `host.plugin_names()`: a trivial command that needs the
//! VM thread. "Responsive" = it answers within PROBE.
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::presentation::TextProvider;
use rness_kernel::EventBus;
use rness_lua::plugin_host::LuaHost;
use rness_protocol::events::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;

const PROBE: Duration = Duration::from_secs(3);

/// Time for the VM to answer a trivial request, or None if it did not
/// answer within `within`.
async fn probe_within(host: &LuaHost, within: Duration) -> Option<Duration> {
    let t = Instant::now();
    tokio::time::timeout(within, host.plugin_names())
        .await
        .ok()
        .map(|_| t.elapsed())
}

async fn responsive(host: &LuaHost) -> bool {
    probe_within(host, PROBE).await.is_some()
}

struct OneAnswer;

#[async_trait]
impl Provider for OneAnswer {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, _request: StepRequest<'_>, _cancel: &CancellationToken) -> StepOutcome {
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

/// A host with a mounted engine (commands, tasks, session APIs).
async fn engine_host() -> (LuaHost, Arc<SessionService>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(OneAnswer),
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
        "fake-1".into(),
    )
    .await
    .unwrap();
    (host, sessions, dir)
}

fn text(t: &str) -> Vec<ContentPart> {
    vec![ContentPart::Text { text: t.into() }]
}

// ---------------------------------------------------------------- baseline

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn baseline_probe_is_fast() {
    let host = LuaHost::spawn().unwrap();
    host.load("ok", "rness.hook.on('tick', function() end)")
        .await
        .unwrap();
    host.fire_hook("tick", json!({}));
    let d = probe_within(&host, PROBE).await.expect("idle VM answers");
    assert!(d < Duration::from_millis(200), "{d:?}");
}

// ------------------------------------------------- (a) notification hooks

/// §4 #1 (a): `fire_hook` installs no instruction hook
/// (runtime.rs:1714, plugin_host.rs:1588). A runaway notification handler
/// (turn_start, frame, prompt, session_idle, ...) wedges the VM for good.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_notification_hook_releases_vm() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.hook.on('frame', function() while true do end end)",
    )
    .await
    .unwrap();
    host.fire_hook("frame", json!({"type": "delta"}));
    // Allow 10 s: more than any reasonable per-callback budget.
    let answered = probe_within(&host, Duration::from_secs(10)).await;
    assert!(
        answered.is_some(),
        "BUG: runaway fire_hook handler wedged the VM (no answer in 10 s)"
    );
}

/// Interception hooks (pre_step/pre_tool/...) ARE bounded: the turn's cancel
/// token and HOOK_TIMEOUT (30 s) are checked every 1000 instructions
/// (plugin_host.rs:1502-1505). Cancelling must free the VM at once.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_intercept_hook_stops_on_cancel() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.hook.on('pre_step', function(ev, next) while true do end end)",
    )
    .await
    .unwrap();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c2.cancel();
    });
    let t = Instant::now();
    let r = host
        .intercept("pre_step", json!({"session": "s"}), json!(null), &cancel)
        .await;
    assert!(r.is_err(), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    let d = probe_within(&host, PROBE).await;
    assert!(d.is_some(), "VM still busy after intercept cancel");
}

/// pcall bypass: a hook error raised by the instruction hook is an ordinary
/// Lua error, so `pcall` inside the handler swallows it and the loop goes on.
/// Even the bounded paths (intercept here) can be defeated.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pcall_wrapped_runaway_intercept_still_stops_on_cancel() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.hook.on('pre_step', function(ev, next)
           while true do pcall(function() while true do end end) end
         end)",
    )
    .await
    .unwrap();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c2.cancel();
    });
    let _ = host
        .intercept("pre_step", json!({"session": "s"}), json!(null), &cancel)
        .await;
    // The caller is released by its own select!, but is the VM?
    let d = probe_within(&host, Duration::from_secs(5)).await;
    assert!(
        d.is_some(),
        "BUG: pcall swallows the cancel error; VM keeps spinning after cancel"
    );
}

// ------------------------------------------------------------- (b) statusline

/// §4 #1 (b): status_view/statusline have no instruction hook
/// (runtime.rs:1801, :1821). The TUI's poll task awaits this with no timeout
/// (rness-cli main.rs:2182 -> ext_statusline.rs:59).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_statusline_is_bounded() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.ui.statusline(function() while true do end end)",
    )
    .await
    .unwrap();
    let t = Instant::now();
    let view = tokio::time::timeout(Duration::from_secs(10), host.status(json!({}))).await;
    assert!(
        view.is_ok(),
        "BUG: runaway statusline never returned (waited {:?})",
        t.elapsed()
    );
    assert!(responsive(&host).await);
}

/// Table-form statusline (`rness.ui.statusline = { left = fn }`): same path.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_table_statusline_is_bounded() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.ui.statusline = { left = function() while true do end end }",
    )
    .await
    .unwrap();
    let view = tokio::time::timeout(Duration::from_secs(10), host.status(json!({}))).await;
    assert!(view.is_ok(), "BUG: runaway table statusline never returned");
}

// ------------------------------------------------------------- (c) tool card

/// Tool cards: 100 ms deadline checked every 10 000 instructions
/// (runtime.rs:1888-1907).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_tool_card_aborted_near_100ms() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.ui.tool_card('loop', function() while true do end end)",
    )
    .await
    .unwrap();
    let t = Instant::now();
    let card = tokio::time::timeout(
        Duration::from_secs(10),
        host.tool_card("loop", json!({}), "", false),
    )
    .await
    .expect("card renderer bounded");
    let took = t.elapsed();
    eprintln!(
        "{{\"probe\":\"runaway_tool_card_abort\",\"ms\":{}}}",
        took.as_millis()
    );
    assert!(card.is_none());
    assert!(took < Duration::from_millis(1000), "{took:?}");
    assert!(responsive(&host).await);
}

/// Same renderer wrapped in pcall: the deadline error is swallowed and the
/// renderer loops forever (the hook keeps raising, pcall keeps catching).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pcall_wrapped_runaway_tool_card_is_bounded() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.ui.tool_card('loop', function()
           while true do pcall(function() while true do end end) end
         end)",
    )
    .await
    .unwrap();
    let card = tokio::time::timeout(
        Duration::from_secs(10),
        host.tool_card("loop", json!({}), "", false),
    )
    .await;
    assert!(
        card.is_ok(),
        "BUG: pcall defeats the 100 ms tool-card deadline"
    );
}

/// Native work is invisible to instruction-count hooks: one long C call
/// (pathological Lua pattern) cannot be interrupted mid-call (B3-6,
/// documented). The budget applies again as soon as the call returns, so
/// the VM is never wedged; the overrun is the length of one C call.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn native_heavy_tool_card_overruns_deadline() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "slow",
        "rness.ui.tool_card('slow', function(call)
           -- O(n^4) backtracking inside one string.find call
           local s = string.rep('a', 120)
           string.find(s, '.-.-.-.-b')
           return {'done'}
         end)",
    )
    .await
    .unwrap();
    let t = Instant::now();
    let card = tokio::time::timeout(
        Duration::from_secs(120),
        host.tool_card("slow", json!({}), "", false),
    )
    .await
    .expect("finishes eventually");
    let took = t.elapsed();
    eprintln!(
        "{{\"probe\":\"native_tool_card_overrun\",\"ms\":{},\"rendered\":{}}}",
        took.as_millis(),
        card.is_some()
    );
    // Known limitation: the C call itself runs to completion, and the few
    // instructions after it may finish the render. The overrun still counts
    // as a strike, and the VM is free right after.
    assert!(took > Duration::from_millis(100), "{took:?}");
    assert!(responsive(&host).await);
}

// ------------------------------------------------------------- (d) command

/// Commands only check their cancel token (runtime.rs:1021). While a runaway
/// command spins, the whole VM (statusline, hooks, cards, other commands)
/// is blocked; Ctrl-C (session cancel) must free it.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_command_blocks_vm_until_cancel() {
    let (host, sessions, _dir) = engine_host().await;
    host.load(
        "spin",
        "rness.commands.register{name='spin', description='x', run=function() while true do end end}",
    )
    .await
    .unwrap();
    let id = sessions.create(None).unwrap();
    let run = tokio::spawn(sessions.send_async(id.clone(), UserIntent::Followup, text("/spin")));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let blocked = probe_within(&host, Duration::from_secs(1)).await;
    eprintln!("command spinning: probe answered = {:?}", blocked);
    sessions.cancel(&id);
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("command returns after cancel")
        .unwrap();
    assert!(result.is_err(), "{result:?}");
    assert!(responsive(&host).await, "VM free after command cancel");
    // Documented, not asserted as a bug: no per-command time budget, so the
    // VM is unavailable for as long as the user does not press Ctrl-C.
    assert!(
        blocked.is_none(),
        "unexpected: VM answered while a command spun"
    );
}

/// pcall-wrapped runaway command: cancel raises inside pcall and is caught.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pcall_wrapped_runaway_command_stops_on_cancel() {
    let (host, sessions, _dir) = engine_host().await;
    host.load(
        "spin",
        "rness.commands.register{name='spin', description='x', run=function()
           while true do pcall(function() while true do end end) end
         end}",
    )
    .await
    .unwrap();
    let id = sessions.create(None).unwrap();
    let run = tokio::spawn(sessions.send_async(id.clone(), UserIntent::Followup, text("/spin")));
    tokio::time::sleep(Duration::from_millis(300)).await;
    sessions.cancel(&id);
    let returned = tokio::time::timeout(Duration::from_secs(5), run).await;
    let free = responsive(&host).await;
    assert!(
        returned.is_ok() && free,
        "BUG: pcall swallows 'command cancelled'; Ctrl-C cannot stop the command (returned={}, vm_free={free})",
        returned.is_ok()
    );
}

// ------------------------------------------------------------- (e) timer

/// Timers: own 1 s budget (B3-7; was HOOK_TIMEOUT, 30 s, during which the
/// TUI's statusline / app roster froze).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_timer_aborted_after_1s() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.timer.after(0.05, function() while true do end end)",
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t = Instant::now();
    let d = probe_within(&host, Duration::from_secs(45)).await;
    let stall = t.elapsed();
    eprintln!(
        "{{\"probe\":\"runaway_timer_stall\",\"ms\":{}}}",
        stall.as_millis()
    );
    assert!(d.is_some(), "timer never aborted");
    assert!(stall < Duration::from_secs(2), "{stall:?}");
}

/// A repeating timer that overruns every time is cancelled after
/// `budget::STRIKES` overruns; the VM stops paying for it.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn repeating_runaway_timer_is_disabled_after_strikes() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.timer.every(1, function() while true do end end)",
    )
    .await
    .unwrap();
    // Three 1 s overruns (one per second), then the timer is gone.
    tokio::time::sleep(Duration::from_millis(7000)).await;
    for _ in 0..8 {
        let d = probe_within(&host, Duration::from_millis(500)).await;
        assert!(
            d.is_some(),
            "VM still busy: repeating runaway timer not disabled"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A notification handler that overruns on every event is unsubscribed
/// after `budget::STRIKES` overruns; other handlers keep running.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_notification_hook_is_disabled_after_strikes() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.hook.on('frame', function() while true do end end)
         ok_count = 0
         rness.hook.on('frame', function() ok_count = ok_count + 1 end)
         rness.tool.register{name='n', run=function() return tostring(ok_count) end}",
    )
    .await
    .unwrap();
    for _ in 0..3 {
        host.fire_hook("frame", json!({}));
    }
    let _ = probe_within(&host, Duration::from_secs(10)).await;
    let t = Instant::now();
    host.fire_hook("frame", json!({}));
    let d = probe_within(&host, PROBE).await.expect("VM answers");
    assert!(
        t.elapsed() < Duration::from_millis(300),
        "{d:?}: handler still runs"
    );
    let n = host.call_tool("n", json!({})).await.unwrap();
    assert_eq!(n, "4", "healthy handler kept running");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pcall_wrapped_runaway_timer_is_bounded() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.timer.after(0.05, function()
           while true do pcall(function() while true do end end) end
         end)",
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let d = probe_within(&host, Duration::from_secs(40)).await;
    assert!(d.is_some(), "BUG: pcall defeats the 30 s timer deadline");
}

// ------------------------------------------------------------- tasks

/// Tasks: ~30 M instruction budget per resume (api/task.rs:241-253).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_task_aborted_by_budget() {
    let (host, _sessions, _dir) = engine_host().await;
    host.load(
        "spin",
        "rness.hook.on('go', function() rness.task.spawn(function() while true do end end) end)",
    )
    .await
    .unwrap();
    host.fire_hook("go", json!({}));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let t = Instant::now();
    let d = probe_within(&host, Duration::from_secs(30)).await;
    eprintln!(
        "{{\"probe\":\"runaway_task_stall\",\"ms\":{}}}",
        t.elapsed().as_millis()
    );
    assert!(d.is_some(), "task budget did not stop the loop");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pcall_wrapped_runaway_task_is_bounded() {
    let (host, _sessions, _dir) = engine_host().await;
    host.load(
        "spin",
        "rness.hook.on('go', function() rness.task.spawn(function()
           while true do pcall(function() while true do end end) end
         end) end)",
    )
    .await
    .unwrap();
    host.fire_hook("go", json!({}));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let d = probe_within(&host, Duration::from_secs(30)).await;
    assert!(
        d.is_some(),
        "BUG: pcall defeats the task instruction budget"
    );
}

// ------------------------------------------------------------- apps / actions

/// App view callbacks (`rness.ui.app{view=}`) have no hook (runtime.rs:1269).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_app_view_is_bounded() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "rness.ui.app{name='spin', slot='overlay', view=function() while true do end end}",
    )
    .await
    .unwrap();
    let r = tokio::time::timeout(Duration::from_secs(10), host.app_view("spin", json!({}))).await;
    assert!(r.is_ok(), "BUG: runaway app view wedged the VM");
}

/// Plugin actions bound to keys (`plugin.action`) have no hook either
/// (runtime.rs:842).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_action_is_bounded() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "spin",
        "local plugin = __rness_plugin_context({})
         plugin.action('go', {scope='promptbox', description='x', run=function() while true do end end})
         plugin.__finish()",
    )
    .await
    .unwrap();
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        host.call_action("spin.go", "promptbox", json!({})),
    )
    .await;
    assert!(r.is_ok(), "BUG: runaway action wedged the VM");
}

// ------------------------------------------------------------- load / reload

/// Plugin top-level code runs with no hook (runtime.rs:395 `exec()`); a
/// runaway chunk hangs startup (before the TUI draws) or a hot reload.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn runaway_plugin_load_is_bounded() {
    let host = LuaHost::spawn().unwrap();
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        host.load("spin", "while true do end"),
    )
    .await;
    assert!(
        r.is_ok(),
        "BUG: runaway plugin chunk hangs load/reload forever"
    );
}

// ------------------------------------------------------------- blocking IO

/// `rness.http` blocks the VM thread (api/http.rs:31-47, `block_on`) for up
/// to its default 30 s timeout when called from a hook. Not a spin, but the
/// same symptom: every other Lua consumer waits.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn http_in_hook_blocks_vm_for_default_timeout() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    // Accept and never answer.
    let _keep = std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming() {
            held.push(s);
        }
    });
    let host = LuaHost::spawn().unwrap();
    host.load(
        "slow",
        &format!("rness.hook.on('tick', function() pcall(rness.http.get, {url:?}) end)"),
    )
    .await
    .unwrap();
    host.fire_hook("tick", json!({}));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t = Instant::now();
    let d = probe_within(&host, Duration::from_secs(40)).await;
    let stall = t.elapsed();
    eprintln!(
        "{{\"probe\":\"http_hook_stall\",\"ms\":{}}}",
        stall.as_millis()
    );
    assert!(d.is_some());
    assert!(
        stall < Duration::from_secs(5),
        "hook calling rness.http on a stalled server blocked the VM for {stall:?}"
    );
}

/// `rness.process.run` likewise blocks the VM (api/process.rs:83-121) up to
/// its 30 s default timeout when called outside a command/tool.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn process_in_hook_blocks_vm() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "slow",
        "rness.hook.on('tick', function() pcall(rness.process.run, {program='sleep', args={'4'}}) end)",
    )
    .await
    .unwrap();
    host.fire_hook("tick", json!({}));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t = Instant::now();
    let d = probe_within(&host, Duration::from_secs(10)).await;
    let stall = t.elapsed();
    eprintln!(
        "{{\"probe\":\"process_hook_stall\",\"ms\":{}}}",
        stall.as_millis()
    );
    assert!(d.is_some());
    assert!(
        stall < Duration::from_secs(1),
        "hook calling rness.process.run blocked the VM for {stall:?}"
    );
}
