//! WP-3 hot-reload scenarios: `require` cache, reload storms, broken
//! rewrites, dependency removal, repeated reloads (hook/timer duplication,
//! memory growth), startup-module edits.
//!
//!   timeout 300 cargo test -p rness-lua --test reload_e2e -- --test-threads 2
use std::sync::Arc;
use std::time::{Duration, Instant};

use rness_lua::loader::PluginSource;
use rness_lua::plugin_host::LuaHost;
use rness_lua::reload::{watch, ReloadReport};
use serde_json::json;

fn src(name: &str, source: &str) -> PluginSource {
    PluginSource {
        name: name.into(),
        source: source.into(),
        dependencies: vec![],
    }
}

async fn tool(host: &LuaHost, name: &str) -> String {
    host.call_tool(name, json!({}))
        .await
        .unwrap_or_else(|e| format!("ERR:{e}"))
}

/// `require`d helpers under `<root>/lua/` stay cached across plugin reloads
/// (package.loaded is part of the retained VM; runtime.rs reload_plugins
/// reuses `self.lua`). Documented in loading-and-lifecycle.md:117. This test
/// pins the behaviour: an edited helper is NOT picked up by a reload, and
/// the watcher reports "restart rness" for edits under lua/.
#[tokio::test(flavor = "multi_thread")]
async fn require_cache_survives_reload_and_lua_dir_edit_requests_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("lua")).unwrap();
    std::fs::create_dir_all(root.join("plugins")).unwrap();
    std::fs::write(root.join("init.lua"), "").unwrap();
    std::fs::write(root.join("lua/helper.lua"), "return {v='v1'}").unwrap();
    let plugin = "local h = require('helper')\n\
                  rness.tool.register{name='which', run=function() return h.v .. '/' .. tostring(package.loaded.helper ~= nil) end}";
    std::fs::write(root.join("plugins/user.lua"), plugin).unwrap();

    let (host, _startup) = LuaHost::spawn_from_init(root.join("init.lua")).unwrap();
    let names = vec!["user".to_string()];
    let sources = rness_lua::loader::discover(&root, &names).unwrap();
    rness_lua::loader::load_all(&host, &sources).await;
    assert_eq!(tool(&host, "which").await, "v1/true");

    let registry = Arc::new(rness_engine::tools::ToolRegistry::default());
    let installed = rness_lua::api::tools::sync_lua_tools(&registry, &host, &[]).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _w = watch(
        root.clone(),
        names.clone(),
        host.clone(),
        registry,
        installed,
        move |r| {
            let _ = tx.send(r);
        },
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 1) edit the helper only -> watcher refuses ("restart rness").
    std::fs::write(root.join("lua/helper.lua"), "return {v='v2'}").unwrap();
    let r = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("report")
        .unwrap();
    eprintln!("helper edit -> {r:?}");
    assert!(
        matches!(&r, ReloadReport::Failed(m) if m.contains("restart")),
        "{r:?}"
    );
    assert_eq!(tool(&host, "which").await, "v1/true");

    // 2) now touch the plugin -> real reload, but require() is still cached.
    // FSEvents may redeliver the earlier lua/ event with later batches, so
    // allow a few edits; record how many were misreported as startup edits.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    while rx.try_recv().is_ok() {}
    let mut misreported = 0;
    let mut reloaded = false;
    for attempt in 0..5 {
        std::fs::write(
            root.join("plugins/user.lua"),
            format!("{plugin}\n-- touched {attempt}"),
        )
        .unwrap();
        let r = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("report")
            .unwrap();
        eprintln!("plugin edit {attempt} -> {r:?}");
        if matches!(r, ReloadReport::Reloaded { .. }) {
            reloaded = true;
            break;
        }
        misreported += 1;
        tokio::time::sleep(Duration::from_millis(500)).await;
        while rx.try_recv().is_ok() {}
    }
    eprintln!("{{\"probe\":\"plugin_edit_misreported_as_startup\",\"n\":{misreported}}}");
    assert!(reloaded, "plugin edits never reloaded after a lua/ edit");
    let after = tool(&host, "which").await;
    eprintln!("{{\"probe\":\"require_cache_after_reload\",\"value\":\"{after}\"}}");
    assert_eq!(
        after, "v1/true",
        "require cache is expected to persist (documented)"
    );

    // 3) A plugin that clears its own cache entry picks up the new helper.
    std::fs::write(
        root.join("plugins/user.lua"),
        format!("package.loaded.helper = nil\n{plugin}"),
    )
    .unwrap();
    for _ in 0..5 {
        if let Ok(Some(ReloadReport::Reloaded { .. })) =
            tokio::time::timeout(Duration::from_secs(10), rx.recv()).await
        {
            break;
        }
    }
    assert_eq!(tool(&host, "which").await, "v2/true");
    assert_eq!(
        misreported, 0,
        "plugin edits after a lua/ edit were misreported as startup changes"
    );
}

/// A broken rewrite keeps the previous hooks, statusline and tools.
#[tokio::test(flavor = "multi_thread")]
async fn broken_reload_keeps_hooks_statusline_and_tools() {
    let host = LuaHost::spawn().unwrap();
    let good = "hits = hits or 0\n\
                rness.hook.on('tick', function() hits = hits + 1 end)\n\
                rness.ui.statusline(function() return 'good' end)\n\
                rness.tool.register{name='hits', run=function() return tostring(hits) end}";
    host.reload(vec![src("p", good)]).await.unwrap();
    for (label, bad) in [
        ("syntax", "this is not lua"),
        (
            "runtime",
            "rness.hook.on('tick', function() hits = hits + 100 end); error('boom')",
        ),
        ("bad-register", "rness.tool.register{name=42}"),
    ] {
        let r = host.reload(vec![src("p", bad)]).await;
        eprintln!("{label}: {r:?}");
        // reload() reports per-plugin errors either as Err or Ok(errors).
        assert!(
            matches!(&r, Err(_)) || matches!(&r, Ok(e) if !e.is_empty()),
            "{label}: {r:?}"
        );
        assert_eq!(host.statusline().await.as_deref(), Some("good"), "{label}");
        host.fire_hook("tick", json!({}));
    }
    // Exactly one live handler: 3 ticks -> 3 (a leaked handler from the
    // failed `runtime` rewrite would add 100).
    assert_eq!(tool(&host, "hits").await, "3");
}

/// Reloading N times must not duplicate hook handlers or timers.
#[tokio::test(flavor = "multi_thread")]
async fn repeated_reload_does_not_duplicate_hooks_or_timers() {
    let host = LuaHost::spawn().unwrap();
    let source = "hits = hits or 0; fires = fires or 0\n\
        rness.hook.on('tick', function() hits = hits + 1 end)\n\
        rness.timer.every(1, function() fires = fires + 1 end)\n\
        rness.tool.register{name='counts', run=function() return hits .. ',' .. fires end}";
    for _ in 0..25 {
        host.reload(vec![src("p", source)]).await.unwrap();
    }
    host.load("reset", "hits = 0; fires = 0").await.unwrap();
    host.fire_hook("tick", json!({}));
    tokio::time::sleep(Duration::from_millis(3300)).await;
    let counts = tool(&host, "counts").await;
    eprintln!("{{\"probe\":\"after_25_reloads\",\"hits,fires\":\"{counts}\"}}");
    let (hits, fires) = counts.split_once(',').unwrap();
    assert_eq!(hits, "1", "hook handler duplicated across reloads");
    let fires: u32 = fires.parse().unwrap();
    assert!(
        (2..=4).contains(&fires),
        "timer duplicated across reloads: {fires} fires in 3.3s"
    );
}

/// Memory after many reloads of a plugin that builds a moderately large
/// table: the old closures must be collectable.
#[tokio::test(flavor = "multi_thread")]
async fn repeated_reload_memory_is_bounded() {
    let host = LuaHost::spawn().unwrap();
    let source = "local big = {} for i=1,20000 do big[i] = 'x' .. i end\n\
        rness.hook.on('tick', function() return #big end)\n\
        rness.ui.tool_card('Mem', function() return {'mem'} end)\n\
        rness.tool.register{name='mem', run=function() collectgarbage('collect'); collectgarbage('collect'); return tostring(math.floor(collectgarbage('count'))) end}";
    host.reload(vec![src("p", source)]).await.unwrap();
    let base: f64 = tool(&host, "mem").await.parse().unwrap();
    let n = std::env::var("RNESS_BENCH_RELOADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let t = Instant::now();
    for _ in 0..n {
        host.reload(vec![src("p", source)]).await.unwrap();
    }
    let per = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
    let after: f64 = tool(&host, "mem").await.parse().unwrap();
    eprintln!(
        "{{\"probe\":\"reload_cycle\",\"n\":{n},\"mean_ms\":{per:.2},\"kb_before\":{base},\"kb_after\":{after}}}"
    );
    assert!(
        after < base * 2.0 + 1024.0,
        "Lua heap grew {base} KiB -> {after} KiB over {n} reloads"
    );
}

/// Removing a prerequisite plugin while a dependant remains declared.
#[tokio::test(flavor = "multi_thread")]
async fn reload_with_missing_dependency_fails_cleanly() {
    let host = LuaHost::spawn().unwrap();
    let a = src(
        "a",
        "rness.tool.register{name='a', run=function() return 'a' end}",
    );
    let mut b = src(
        "b",
        "rness.tool.register{name='b', run=function() return 'b' end}",
    );
    b.dependencies = vec!["a".into()];
    host.reload(vec![a, b.clone()]).await.unwrap();
    let r = host.reload(vec![b]).await;
    eprintln!("dependency removed -> {r:?}");
    assert!(r.is_err(), "{r:?}");
    assert_eq!(tool(&host, "a").await, "a", "previous set kept");
    assert_eq!(tool(&host, "b").await, "b");
}

/// Dependency cycle is rejected before anything is unloaded.
#[tokio::test(flavor = "multi_thread")]
async fn reload_with_cycle_keeps_previous_set() {
    let host = LuaHost::spawn().unwrap();
    host.reload(vec![src(
        "a",
        "rness.tool.register{name='a', run=function() return 'a' end}",
    )])
    .await
    .unwrap();
    let mut a = src("a", "");
    a.dependencies = vec!["b".into()];
    let mut b = src("b", "");
    b.dependencies = vec!["a".into()];
    let r = host.reload(vec![a, b]).await;
    eprintln!("cycle -> {r:?}");
    assert!(r.is_err());
    assert_eq!(tool(&host, "a").await, "a");
}

/// Editor-style storm: many rapid writes. The watcher must converge on the
/// final content and not run one reload per write.
#[tokio::test(flavor = "multi_thread")]
async fn reload_storm_converges_on_last_write() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("plugins")).unwrap();
    let body =
        |v: usize| format!("rness.tool.register{{name='v', run=function() return '{v}' end}}");
    std::fs::write(root.join("plugins/p.lua"), body(0)).unwrap();
    let names = vec!["p".to_string()];
    let host = LuaHost::spawn().unwrap();
    let sources = rness_lua::loader::discover(&root, &names).unwrap();
    rness_lua::loader::load_all(&host, &sources).await;
    let registry = Arc::new(rness_engine::tools::ToolRegistry::default());
    let installed = rness_lua::api::tools::sync_lua_tools(&registry, &host, &[]).await;
    let reports = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = reports.clone();
    let _w = watch(
        root.clone(),
        names,
        host.clone(),
        registry.clone(),
        installed,
        move |r| {
            sink.lock().unwrap().push(r);
        },
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let n = std::env::var("RNESS_BENCH_STORM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let t = Instant::now();
    for v in 1..=n {
        // write + rename, like editors' atomic save
        let tmp = root.join("plugins/.p.lua.tmp");
        std::fs::write(&tmp, body(v)).unwrap();
        std::fs::rename(&tmp, root.join("plugins/p.lua")).unwrap();
        if v % 10 == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if tool(&host, "v").await == n.to_string() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "watcher never converged; last={}",
            tool(&host, "v").await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    let reports = reports.lock().unwrap();
    let failed = reports
        .iter()
        .filter(|r| matches!(r, ReloadReport::Failed(_)))
        .count();
    eprintln!(
        "{{\"probe\":\"reload_storm\",\"writes\":{n},\"reloads\":{},\"failed\":{failed},\"converge_ms\":{}}}",
        reports.len(),
        t.elapsed().as_millis()
    );
    assert_eq!(failed, 0, "{reports:?}");
    assert!(
        reports.len() < n / 2,
        "debounce ineffective: {} reloads for {n} writes",
        reports.len()
    );
    assert_eq!(registry.names(), vec!["v"]);
}

/// Deleting a watched plugin file: reported, previous registrations kept.
#[tokio::test(flavor = "multi_thread")]
async fn deleted_plugin_file_reports_and_keeps_tools() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("plugins")).unwrap();
    std::fs::write(
        root.join("plugins/p.lua"),
        "rness.tool.register{name='p', run=function() return 'p' end}",
    )
    .unwrap();
    let names = vec!["p".to_string()];
    let host = LuaHost::spawn().unwrap();
    rness_lua::loader::load_all(&host, &rness_lua::loader::discover(&root, &names).unwrap()).await;
    let registry = Arc::new(rness_engine::tools::ToolRegistry::default());
    let installed = rness_lua::api::tools::sync_lua_tools(&registry, &host, &[]).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _w = watch(
        root.clone(),
        names,
        host.clone(),
        registry.clone(),
        installed,
        move |r| {
            let _ = tx.send(r);
        },
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::remove_file(root.join("plugins/p.lua")).unwrap();
    let r = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("report")
        .unwrap();
    eprintln!("delete -> {r:?}");
    assert!(matches!(r, ReloadReport::Failed(_)));
    assert_eq!(registry.names(), vec!["p"]);
    assert_eq!(tool(&host, "p").await, "p");
}

/// Hooks firing during a reload: frames queued around a reload must all be
/// handled by exactly one generation (no drops, no doubles).
#[tokio::test(flavor = "multi_thread")]
async fn hook_storm_during_reload_counts_once() {
    let host = LuaHost::spawn().unwrap();
    let source = "n = n or 0\nrness.hook.on('frame', function() n = n + 1 end)\n\
                  rness.tool.register{name='n', run=function() return tostring(n) end}";
    host.reload(vec![src("p", source)]).await.unwrap();
    let firing = host.clone();
    let fire = tokio::spawn(async move {
        for _ in 0..5000 {
            firing.fire_hook("frame", json!({"type": "delta"}));
            tokio::task::yield_now().await;
        }
    });
    for _ in 0..20 {
        host.reload(vec![src("p", source)]).await.unwrap();
    }
    fire.await.unwrap();
    assert_eq!(tool(&host, "n").await, "5000");
}
