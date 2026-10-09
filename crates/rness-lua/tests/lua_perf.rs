//! WP-3 Lua host perf probes. All `#[ignore]`; run in release:
//!
//!   cargo test -p rness-lua --release --test lua_perf -- --ignored --nocapture --test-threads 1
//!
//! Each probe prints one JSON line {"probe":..., "n":..., "median_ms":..., ...}.
//! Sizes: RNESS_BENCH_N (iterations, default 2000), RNESS_BENCH_HOOKS
//! (handlers for the fan-out probe, default 20), RNESS_BENCH_TASKS (default 50).
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rness_kernel::presentation::TextProvider;
use rness_lua::plugin_host::LuaHost;
use serde_json::json;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn stats(samples: &mut [Duration]) -> (f64, f64, f64) {
    samples.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let n = samples.len();
    (
        ms(samples[n / 2]),
        ms(samples[(n * 95 / 100).min(n - 1)]),
        ms(samples[(n * 99 / 100).min(n - 1)]),
    )
}

fn report(probe: &str, samples: &mut [Duration], extra: serde_json::Value) {
    let n = samples.len();
    let (median, p95, p99) = stats(samples);
    let mut line = json!({"probe": probe, "n": n, "median_ms": round(median), "p95_ms": round(p95), "p99_ms": round(p99)});
    if let (Some(obj), Some(more)) = (line.as_object_mut(), extra.as_object()) {
        obj.extend(more.clone());
    }
    println!("{line}");
}

fn round(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

const STUBS: &str = "rness.session = rness.session or {}\n\
    rness.session.usage = rness.session.usage or function() return {input = 12345, cache_read = 100, cache_write = 0} end\n\
    rness.subagents = rness.subagents or {list = function() return {} end}";

fn flavor() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default")
}

/// A delta frame shaped like the engine's FrameEv for text streaming.
fn delta(i: usize) -> serde_json::Value {
    json!({"type": "delta", "session": "01SESSION", "step": 1,
           "chunk": {"d": "text", "text": format!("token {i} ")}})
}

/// Round trip of a trivial request: the floor for every TUI→Lua call.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_vm_round_trip() {
    let host = LuaHost::spawn().unwrap();
    let n = env("RNESS_BENCH_N", 2000);
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        host.plugin_names().await;
        samples.push(t.elapsed());
    }
    report("vm_round_trip", &mut samples, json!({}));
}

/// fire_hook("frame") throughput through the real default-flavor statusline
/// plugin handler (string match + table writes per delta).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_frame_hook_throughput_flavor_statusline() {
    let host = LuaHost::spawn().unwrap();
    let src = std::fs::read_to_string(flavor().join("plugins/statusline.lua")).unwrap();
    host.load("statusline", &src).await.unwrap();
    let n = env("RNESS_BENCH_N", 2000) * 10;
    let t = Instant::now();
    for i in 0..n {
        host.fire_hook("frame", delta(i));
    }
    // Drain: a round trip completes after every queued FireHook.
    host.plugin_names().await;
    let total = t.elapsed();
    println!(
        "{}",
        json!({"probe": "frame_hook_flavor_statusline", "n": n,
               "median_ms": round(total.as_secs_f64() * 1000.0 / n as f64),
               "total_ms": round(total.as_secs_f64() * 1000.0),
               "frames_per_s": (n as f64 / total.as_secs_f64()).round()})
    );
}

/// Same with K independent handlers on `frame`: cost per handler.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_frame_hook_fanout() {
    let host = LuaHost::spawn().unwrap();
    let k = env("RNESS_BENCH_HOOKS", 20);
    for i in 0..k {
        host.load(
            &format!("h{i}"),
            "local seen = 0; rness.hook.on('frame', function(f) if f.type == 'delta' then seen = seen + 1 end end)",
        )
        .await
        .unwrap();
    }
    let n = env("RNESS_BENCH_N", 2000) * 5;
    let t = Instant::now();
    for i in 0..n {
        host.fire_hook("frame", delta(i));
    }
    host.plugin_names().await;
    let total = t.elapsed();
    println!(
        "{}",
        json!({"probe": "frame_hook_fanout", "n": n, "handlers": k,
               "median_ms": round(total.as_secs_f64() * 1000.0 / n as f64),
               "us_per_handler_call": round(total.as_secs_f64() * 1e6 / (n * k) as f64)})
    );
}

/// Baseline: fire_hook for an event nobody listens to (serialisation +
/// channel + lookup). Shows the fixed cost paid per bus event.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_fire_hook_no_listener() {
    let host = LuaHost::spawn().unwrap();
    let n = env("RNESS_BENCH_N", 2000) * 10;
    let t = Instant::now();
    for i in 0..n {
        host.fire_hook("frame", delta(i));
    }
    host.plugin_names().await;
    let total = t.elapsed();
    println!(
        "{}",
        json!({"probe": "fire_hook_no_listener", "n": n,
               "median_ms": round(total.as_secs_f64() * 1000.0 / n as f64)})
    );
}

/// Full status_view through the default flavor's structured statusline,
/// the call the TUI makes every 500 ms (main.rs:2182). Includes
/// `rness.subagents` / session lookups only if mounted (not here).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_status_view_flavor() {
    let host = LuaHost::spawn().unwrap();
    let src = std::fs::read_to_string(flavor().join("plugins/statusline.lua")).unwrap();
    // Unmounted host: give the plugin the session/subagent APIs it pcalls.
    host.load("stubs", STUBS).await.unwrap();
    host.load("statusline", &src).await.unwrap();
    host.fire_hook("turn_start", json!({"session": "01SESSION"}));
    let ctx = json!({"session": "01SESSION", "model": "anthropic/fake", "busy": true, "activity": "streaming",
                     "elapsed_ms": 1234, "elapsed": "1s"});
    let n = env("RNESS_BENCH_N", 2000);
    let mut samples = Vec::with_capacity(n);
    let mut last = None;
    for _ in 0..n {
        let t = Instant::now();
        let v = host.status(ctx.clone()).await;
        samples.push(t.elapsed());
        last = v;
    }
    let shown = last
        .as_ref()
        .map(|v| v.to_string().chars().take(100).collect::<String>());
    report("status_view_flavor", &mut samples, json!({"view": shown}));
    assert!(
        last.is_some(),
        "flavor statusline returned nothing (see ~warn log)"
    );
}

/// Statusline latency while a frame storm is being processed: what the
/// user sees as status-bar lag during fast streaming.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_status_view_under_frame_storm() {
    let host = LuaHost::spawn().unwrap();
    let src = std::fs::read_to_string(flavor().join("plugins/statusline.lua")).unwrap();
    host.load("statusline", &src).await.unwrap();
    let ctx = json!({"session": "01SESSION", "model": "m", "busy": true});
    let firing = host.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop.clone();
    // ~5000 frames/s producer (bursty, like a fast stream with tool args).
    let producer = tokio::spawn(async move {
        let mut i = 0;
        while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
            for _ in 0..50 {
                firing.fire_hook("frame", delta(i));
                i += 1;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        i
    });
    let n = env("RNESS_BENCH_N", 2000) / 10;
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        host.status(ctx.clone()).await;
        samples.push(t.elapsed());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let frames = producer.await.unwrap();
    report(
        "status_view_under_frame_storm",
        &mut samples,
        json!({"frames": frames}),
    );
}

/// Unbounded mailbox: a burst of frames when the VM is busy queues without
/// limit. Measures queue drain time and RSS growth for a 200k-frame burst
/// fired while the VM is blocked by a 2 s handler.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_mailbox_backlog() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "slow",
        "rness.hook.on('block', function() local t = os.clock() while os.clock() - t < 2 do end end)\n\
         n = 0; rness.hook.on('frame', function() n = n + 1 end)",
    )
    .await
    .unwrap();
    let rss = || -> u64 {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    };
    let before = rss();
    host.fire_hook("block", json!({}));
    let burst = env("RNESS_BENCH_N", 2000) * 100;
    let t = Instant::now();
    for i in 0..burst {
        host.fire_hook("frame", delta(i));
    }
    let enqueue = t.elapsed();
    let peak = rss();
    let probe = Instant::now();
    host.plugin_names().await;
    let wait = probe.elapsed();
    println!(
        "{}",
        json!({"probe": "mailbox_backlog", "n": burst,
               "median_ms": round(wait.as_secs_f64() * 1000.0),
               "enqueue_ms": round(enqueue.as_secs_f64() * 1000.0),
               "rss_kb_before": before, "rss_kb_peak": peak,
               "kb_per_queued_frame": round((peak.saturating_sub(before)) as f64 / burst as f64)})
    );
}

/// Tool-card render latency: default flavor TaskWrite card, 50 tasks
/// (RNESS_BENCH_TASKS), plus a trivial card for the floor.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_tool_card_render() {
    let host = LuaHost::spawn().unwrap();
    let src = std::fs::read_to_string(flavor().join("plugins/tasks.lua")).unwrap();
    // tasks.lua calls rness.tasks.enable (needs a mounted engine); stub it.
    host.load(
        "tasks",
        &format!("rness.tasks = rness.tasks or {{}}; rness.tasks.enable = function() end\n{src}"),
    )
    .await
    .unwrap();
    host.load(
        "trivial",
        "rness.ui.tool_card('Trivial', function() return {'ok'} end)",
    )
    .await
    .unwrap();
    let k = env("RNESS_BENCH_TASKS", 50);
    let tasks: Vec<_> = (0..k)
        .map(|i| {
            let status = ["pending", "in_progress", "completed"][i % 3];
            json!({"id": i.to_string(), "content": format!("task number {i} with some words"), "status": status})
        })
        .collect();
    let args = json!({"tasks": tasks});
    let n = env("RNESS_BENCH_N", 2000);
    for (probe, name, args) in [
        ("tool_card_trivial", "Trivial", json!({})),
        ("tool_card_flavor_taskwrite", "TaskWrite", args),
    ] {
        let mut samples = Vec::with_capacity(n);
        let mut rendered = false;
        for _ in 0..n {
            let t = Instant::now();
            rendered = host
                .tool_card(name, args.clone(), "ok", false)
                .await
                .is_some();
            samples.push(t.elapsed());
        }
        report(
            probe,
            &mut samples,
            json!({"rendered": rendered, "tasks": k}),
        );
    }
}

/// Large output into a card renderer: 1 MiB tool output marshalled to Lua.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_tool_card_large_output() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "big",
        "rness.ui.tool_card('Big', function(call) return {tostring(#(call.output or ''))} end)",
    )
    .await
    .unwrap();
    let out = "x".repeat(env("RNESS_BENCH_BYTES", 1 << 20));
    let n = env("RNESS_BENCH_N", 2000) / 10;
    let mut samples = Vec::with_capacity(n);
    let mut seen = None;
    for _ in 0..n {
        let t = Instant::now();
        seen = host.tool_card("Big", json!({}), &out, false).await;
        samples.push(t.elapsed());
    }
    let seen = seen.map(|lines| format!("{lines:?}").chars().take(80).collect::<String>());
    report(
        "tool_card_1mib_output",
        &mut samples,
        json!({"bytes": out.len(), "lua_saw": seen}),
    );
}

/// Startup cost: spawn_from_init on the real default flavor init.lua
/// (declares + loads every enabled plugin).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_flavor_spawn_from_init() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join(".rness");
    copy_dir(&flavor(), &cfg);
    let n = env("RNESS_BENCH_N", 2000) / 100;
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n.max(5) {
        let t = Instant::now();
        let (host, startup) = LuaHost::spawn_from_init(cfg.join("init.lua")).unwrap();
        let sources = rness_lua::loader::discover_specs(&cfg, &startup.plugin_specs).unwrap();
        // Unmounted: plugins needing the engine fail; report the count.
        let errors = rness_lua::loader::load_all(&host, &sources).await;
        samples.push(t.elapsed());
        let _ = errors;
    }
    report("flavor_spawn_from_init_unmounted", &mut samples, json!({}));
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&path, &to.join(entry.file_name()));
        } else {
            std::fs::copy(&path, to.join(entry.file_name())).unwrap();
        }
    }
}
