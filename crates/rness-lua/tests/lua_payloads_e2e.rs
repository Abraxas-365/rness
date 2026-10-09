//! WP-3 P2: hostile/bad payloads crossing the Lua <-> Rust boundary and
//! resource storms. Every test is bounded by a tokio timeout; a failure
//! names the boundary that misbehaved.
//!
//!   timeout 600 cargo test -p rness-lua --test lua_payloads_e2e -- --test-threads 2 --nocapture
use std::time::{Duration, Instant};

use rness_kernel::presentation::TextProvider;
use rness_lua::plugin_host::LuaHost;
use serde_json::json;

const T: Duration = Duration::from_secs(20);

async fn alive(host: &LuaHost) -> bool {
    tokio::time::timeout(Duration::from_secs(5), host.plugin_names())
        .await
        .is_ok()
}

async fn tool_result(host: &LuaHost, body: &str) -> Result<String, String> {
    host.load(
        "t",
        &format!("rness.tool.register{{name='t', run=function(args) {body} end}}"),
    )
    .await
    .unwrap();
    let r = tokio::time::timeout(T, host.call_tool("t", json!({})))
        .await
        .expect("tool call bounded");
    host.reload(vec![]).await.ok();
    r
}

/// Tool return values that are hard to serialise.
#[tokio::test(flavor = "multi_thread")]
async fn tool_return_values_are_survivable() {
    let host = LuaHost::spawn().unwrap();
    let cases = [
        ("cyclic", "local t = {}; t.self = t; return t"),
        ("function", "return {f = function() end}"),
        ("nan", "return {x = 0/0}"),
        ("inf", "return {x = math.huge}"),
        ("invalid_utf8", "return '\\xff\\xfe\\xc0'"),
        ("sparse_array", "return {[1]=1, [1000000]=2}"),
        ("mixed_keys", "return {1, 2, a = 3}"),
        ("nil", "return nil"),
        ("boolean", "return false"),
        (
            "userdata_null",
            "return rness.json and rness.json.null or nil",
        ),
        ("error_table", "error({code = 1})"),
        ("error_nil", "error(nil)"),
        ("huge_string_8mb", "return string.rep('x', 8 * 1024 * 1024)"),
    ];
    let mut out = Vec::new();
    for (label, body) in cases {
        let t = Instant::now();
        let r = tool_result(&host, body).await;
        let summary = match &r {
            Ok(s) => format!(
                "ok len={} head={:?}",
                s.len(),
                s.chars().take(60).collect::<String>()
            ),
            Err(e) => format!("err {:?}", e.chars().take(120).collect::<String>()),
        };
        println!(
            "{}",
            json!({"case": label, "ms": t.elapsed().as_millis(), "result": summary})
        );
        out.push((label, r));
        assert!(alive(&host).await, "{label}: VM died");
    }
    let get = |l: &str| out.iter().find(|(x, _)| *x == l).unwrap().1.clone();
    // NaN must not be produced as invalid JSON text.
    if let Ok(s) = get("nan") {
        assert!(
            serde_json::from_str::<serde_json::Value>(&s).is_ok(),
            "tool output is not JSON: {s}"
        );
    }
    // An error table must give the model something readable.
    match get("error_table") {
        Err(e) => assert!(!e.trim().is_empty(), "empty error message"),
        Ok(s) => panic!("error() returned Ok({s})"),
    }
}

/// Hook payloads with odd content survive the JSON -> Lua conversion.
#[tokio::test(flavor = "multi_thread")]
async fn hook_payload_edge_cases() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "h",
        "seen = {}\n\
         rness.hook.on('ev', function(p) seen[#seen+1] = type(p.v) .. ':' .. tostring(p.v == nil) end)\n\
         rness.tool.register{name='seen', run=function() return table.concat(seen, ',') end}",
    )
    .await
    .unwrap();
    let deep = (0..200).fold(json!(1), |acc, _| json!({"n": acc}));
    for v in [
        json!(null),
        json!(u64::MAX),
        json!(-0.0),
        json!("\u{0}nul\u{0}"),
        json!("x".repeat(4 << 20)),
        deep,
        json!([null, null]),
    ] {
        host.fire_hook("ev", json!({"v": v}));
    }
    let seen = tokio::time::timeout(T, host.call_tool("seen", json!({})))
        .await
        .unwrap()
        .unwrap();
    println!("{}", json!({"case": "hook_payloads", "seen": seen}));
    assert_eq!(
        seen.split(',').count(),
        7,
        "some hook payloads were dropped: {seen}"
    );
}

/// Statusline / card return values of the wrong type.
#[tokio::test(flavor = "multi_thread")]
async fn ui_returns_wrong_types() {
    let host = LuaHost::spawn().unwrap();
    for (label, ret) in [
        ("number", "42"),
        ("function", "function() end"),
        (
            "cyclic",
            "(function() local t = {} t[1] = t return t end)()",
        ),
        ("huge", "string.rep('s', 4*1024*1024)"),
        (
            "table_of_junk",
            "{left = 5, right = {{text = function() end}}}",
        ),
    ] {
        host.reload(vec![rness_lua::loader::PluginSource {
            name: "s".into(),
            source: format!(
                "rness.ui.statusline(function() return {ret} end)\n\
                 rness.ui.tool_card('C', function() return {ret} end)"
            ),
            dependencies: vec![],
        }])
        .await
        .unwrap();
        let t = Instant::now();
        let view = tokio::time::timeout(T, host.status(json!({})))
            .await
            .expect("status bounded");
        let card = tokio::time::timeout(T, host.tool_card("C", json!({}), "", false))
            .await
            .expect("card bounded");
        println!(
            "{}",
            json!({"case": label, "ms": t.elapsed().as_millis(),
                   "status_len": view.as_ref().map(|v| v.to_string().len()),
                   "card": card.as_ref().map(|c| c.len())})
        );
        assert!(alive(&host).await, "{label}");
        if label == "huge" {
            // The statusline is one terminal row; a 4 MiB value is passed
            // through to the renderer unbounded.
            let len = view.as_ref().map(|v| v.to_string().len()).unwrap_or(0);
            assert!(len < 64 * 1024, "statusline view is unbounded: {len} bytes");
        }
    }
}

/// Memory bomb in a hook: no memory limit on the main VM (only tool cards
/// get one, runtime.rs:1890). Allocates 1 GiB inside a notification hook.
#[tokio::test(flavor = "multi_thread")]
async fn memory_bomb_in_hook_is_limited() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "bomb",
        "rness.hook.on('bomb', function()\n\
           local t = {}\n\
           for i = 1, 64 do t[i] = string.rep(string.char(64 + i % 26), 16 * 1024 * 1024) end\n\
           kept = t\n\
         end)\n\
         rness.tool.register{name='kb', run=function() return tostring(math.floor(collectgarbage('count'))) end}",
    )
    .await
    .unwrap();
    host.fire_hook("bomb", json!({}));
    let kb: f64 = tokio::time::timeout(Duration::from_secs(60), host.call_tool("kb", json!({})))
        .await
        .unwrap()
        .unwrap()
        .parse()
        .unwrap();
    println!(
        "{}",
        json!({"case": "memory_bomb_hook", "lua_heap_mb": (kb / 1024.0).round()})
    );
    assert!(alive(&host).await);
    assert!(
        kb < 512.0 * 1024.0,
        "no VM memory limit: hook retained {} MiB",
        kb / 1024.0
    );
}

/// Timer storm: many timers due at once, each cheap. The timer thread
/// forwards FireTimer commands; probe latency must stay bounded.
#[tokio::test(flavor = "multi_thread")]
async fn timer_storm_is_bounded() {
    let host = LuaHost::spawn().unwrap();
    let n = std::env::var("RNESS_BENCH_TIMERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20000);
    let r = host
        .load(
            "storm",
            &format!("fired = 0; for i = 1, {n} do rness.timer.after(0.2, function() fired = fired + 1 end) end\n\
                      rness.tool.register{{name='fired', run=function() return tostring(fired) end}}"),
        )
        .await;
    println!(
        "{}",
        json!({"case": "timer_storm_register", "n": n, "result": format!("{r:?}")})
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    let t = Instant::now();
    assert!(alive(&host).await);
    let probe = t.elapsed();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let fired = host.call_tool("fired", json!({})).await.unwrap();
    println!(
        "{}",
        json!({"case": "timer_storm", "n": n, "fired": fired, "probe_ms": probe.as_millis()})
    );
    assert!(
        r.is_err() || fired == n.to_string(),
        "timers lost: {fired}/{n}"
    );
}

/// Task spawn storm (unmounted host: spawn should fail cleanly, not panic).
#[tokio::test(flavor = "multi_thread")]
async fn task_cap_enforced() {
    let host = LuaHost::spawn().unwrap();
    let r = host
        .load(
            "tasks",
            "ok, err = 0, nil\n\
             for i = 1, 1000 do\n\
               local s, e = pcall(rness.task.spawn, function() end)\n\
               if s then ok = ok + 1 else err = e break end\n\
             end\n\
             rness.tool.register{name='n', run=function() return ok .. '|' .. tostring(err) end}",
        )
        .await;
    let out = if r.is_ok() {
        host.call_tool("n", json!({})).await
    } else {
        Err(format!("{r:?}"))
    };
    println!(
        "{}",
        json!({"case": "task_cap", "result": format!("{out:?}")})
    );
    assert!(alive(&host).await);
}

/// Hook registration storm: K handlers (RNESS_BENCH_HOOKS, default 5000) on one event.
/// Registration is quadratic (see wp-3.md B3-10); 100k takes > 280 s.
#[tokio::test(flavor = "multi_thread")]
async fn hook_registration_storm() {
    let k: usize = std::env::var("RNESS_BENCH_HOOKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000);
    let host = LuaHost::spawn().unwrap();
    let t = Instant::now();
    host.load(
        "many",
        &format!(
            "n = 0; for i = 1, {k} do rness.hook.on('x', function() n = n + 1 end) end\n\
         rness.tool.register{{name='n', run=function() return tostring(n) end}}"
        ),
    )
    .await
    .unwrap();
    let reg = t.elapsed();
    let t = Instant::now();
    host.fire_hook("x", json!({}));
    let n = host.call_tool("n", json!({})).await.unwrap();
    let fire = t.elapsed();
    let t = Instant::now();
    host.reload(vec![]).await.unwrap();
    let unload = t.elapsed();
    println!(
        "{}",
        json!({"case": "hook_registration_storm", "handlers": k, "register_ms": reg.as_millis(),
               "fire_ms": fire.as_millis(), "unload_ms": unload.as_millis(), "n": n})
    );
    assert_eq!(n, k.to_string());
    assert!(
        reg + unload < Duration::from_secs(5),
        "{k} hooks: register {reg:?}, unload {unload:?}"
    );
}

/// rness.process.run output cap (1 MiB) and timeout honoured.
#[tokio::test(flavor = "multi_thread")]
async fn process_output_cap_and_timeout() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "p",
        "rness.tool.register{name='big', run=function()\n\
           local r = rness.process.run{program='sh', args={'-c', 'head -c 20000000 /dev/zero'}}\n\
           return #(r.stdout or '') .. '|' .. tostring(r.truncated)\n\
         end}\n\
         rness.tool.register{name='slow', run=function()\n\
           local ok, r = pcall(rness.process.run, {program='sleep', args={'30'}, timeout_ms=300})\n\
           return tostring(ok) .. '|' .. (type(r) == 'table' and rness.json.encode(r) or tostring(r))\n\
         end}",
    )
    .await
    .unwrap();
    let t = Instant::now();
    let big = tokio::time::timeout(T, host.call_tool("big", json!({})))
        .await
        .unwrap();
    let big_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let slow = tokio::time::timeout(T, host.call_tool("slow", json!({})))
        .await
        .unwrap();
    let slow_ms = t.elapsed().as_millis();
    println!(
        "{}",
        json!({"case": "process_cap", "big": format!("{big:?}"), "big_ms": big_ms,
                          "slow": format!("{slow:?}").chars().take(200).collect::<String>(), "slow_ms": slow_ms})
    );
    // Documented (commands.md:217): excessive output raises.
    assert!(
        matches!(&big, Err(e) if e.contains("output limit")),
        "{big:?}"
    );
    assert!(slow_ms < 3000, "timeout_ms ignored: {slow_ms} ms");
}

/// A deeply nested table returned from a tool overflows the lua-vm thread's
/// stack inside serde conversion (`lua_display` -> `from_value`,
/// runtime.rs:3699) and ABORTS THE PROCESS. `#[ignore]`: it kills the test
/// binary; run alone:
///   RNESS_BENCH_DEPTH=10000 cargo test -p rness-lua --test lua_payloads_e2e -- --ignored deep_nested_tool_result
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn deep_nested_tool_result_does_not_abort() {
    let depth: usize = std::env::var("RNESS_BENCH_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10000);
    let host = LuaHost::spawn().unwrap();
    let r = tool_result(
        &host,
        &format!("local t = {{}} local c = t for i=1,{depth} do c.n = {{}} c = c.n end return t"),
    )
    .await;
    println!(
        "{}",
        json!({"case": "deep_nesting", "depth": depth, "ok": r.is_ok()})
    );
    assert!(alive(&host).await);
}
