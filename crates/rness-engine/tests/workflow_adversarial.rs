//! Hostile-script checks: every run must settle (bounded time), and escape
//! attempts must fail the run rather than hang it or leak capabilities.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rness_engine::workflow::{
    self, ChildOutcome, ChildRequest, ChildRunner, WorkflowLimits, WorkflowResult, WorkflowStop,
};
use rness_protocol::events::SessionId;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct Echo;

#[async_trait]
impl ChildRunner for Echo {
    fn check_role(&self, _: Option<&str>) -> Result<(), String> {
        Ok(())
    }

    async fn run(
        &self,
        request: ChildRequest,
        _: CancellationToken,
        started: Box<dyn FnOnce(SessionId) + Send>,
    ) -> ChildOutcome {
        started(format!("child-{}", request.seq));
        ChildOutcome::Completed {
            text: format!("len:{}", request.prompt.len()),
            structured: None,
        }
    }
}

async fn run(script: &str) -> WorkflowResult {
    let meta = workflow::validate_meta(&json!({"name":"adv","description":"d"})).unwrap();
    let limits = WorkflowLimits {
        script_budget: Duration::from_millis(300),
        memory_bytes: 16 * 1024 * 1024,
        dispose_grace: Duration::from_secs(1),
        ..WorkflowLimits::default()
    };
    tokio::time::timeout(
        Duration::from_secs(15),
        workflow::run(
            meta,
            script.into(),
            Value::Null,
            limits,
            Arc::new(Echo),
            Arc::new(|_| {}),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("run must settle: {script}"))
}

fn err(result: &WorkflowResult) -> &str {
    assert_eq!(result.stop, WorkflowStop::Error, "{result:?}");
    result.error.as_deref().unwrap()
}

fn ok(result: &WorkflowResult) -> &Value {
    assert_eq!(result.stop, WorkflowStop::Completed, "{result:?}");
    result.value.as_ref().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn looping_tostring_on_error_objects_cannot_hang_the_run() {
    for script in [
        r#"error(setmetatable({}, { __tostring = function() while true do end end }))"#,
        r#"return parallel({ function() error(setmetatable({}, { __tostring = function() while true do end end })) end })"#,
        r#"local ok, e = pcall(error, setmetatable({}, { __tostring = function() while true do end end })) return tostring(e)"#,
    ] {
        let result = run(script).await;
        let message = err(&result);
        // Stringified inside the hooked coroutine: the ordinary budget
        // error, not the stuck-in-C watchdog.
        assert!(
            message.contains("CPU budget (300 ms"),
            "{script}: {message}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rebinding_globals_cannot_disable_the_guards() {
    let looping = "setmetatable({}, { __tostring = function() while true do end end })";
    for prelude in [
        r#"type = function() return "string" end"#,
        "tostring = function(e) return e end",
        "local E = error; error = function(e) E(e, 0) end",
        r#"setmetatable(_G, { __index = function() return function() return "string" end end })"#,
    ] {
        let script = format!("{prelude}\nerror({looping})");
        let message = err(&run(&script).await).to_owned();
        assert!(
            message.contains("CPU budget (300 ms"),
            "{prelude}: {message}"
        );
    }
    for prelude in [
        "error = function() end",
        "type = function() return 'nil' end",
    ] {
        let script = format!("{prelude}\nsetmetatable({{}}, {{ __gc = function() end }}) return 1");
        let message = err(&run(&script).await).to_owned();
        assert!(message.contains("__gc"), "{prelude}: {message}");
    }
    // A rebound `error` still fails the root, rather than returning nil.
    let result = run("local E = error; error = function() end; E('real')").await;
    assert!(err(&result).contains("real"));
}

#[tokio::test(flavor = "multi_thread")]
async fn callback_errors_render_without_a_doubled_prefix() {
    let message = err(&run(r#"return json.decode("{")"#).await).to_owned();
    assert!(
        message.starts_with("workflow script failed: json.decode:"),
        "{message}"
    );
    assert!(!message.contains("runtime error"), "{message}");
    assert!(!message.contains("stack traceback"), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stack_overflow_is_an_ordinary_error() {
    // Shallow C stack (LUAI_MAXCCALLS) vs. a deep Lua stack: the deep one
    // may hit the memory limit first under a small cap — both are fatal-safe.
    let result = run("local function f() return 1 + f() end return f()").await;
    let message = err(&result);
    assert!(
        message.contains("stack overflow") || message.contains("memory limit"),
        "{message}"
    );
    let result = run(
        "local function f(n) if n == 0 then return 0 end return 1 + f(n - 1) end
         return parallel({ function() return f(250) end, function() return 2 end })",
    )
    .await;
    assert_eq!(ok(&result), &json!([250, 2]));
    let result = run(r#"return parallel({ function() return string.rep("x", -1) .. nil end, function() return 2 end })"#).await;
    assert_eq!(ok(&result), &json!([null, 2]));
}

#[tokio::test(flavor = "multi_thread")]
async fn close_handlers_and_sort_comparators_stay_budgeted() {
    for script in [
        "do local x <close> = setmetatable({}, { __close = function() while true do end end }) end",
        "local t = {3, 2, 1} table.sort(t, function() while true do end end)",
        r#"return string.format("%s", setmetatable({}, { __tostring = function() while true do end end }))"#,
        r#"return (string.gsub("aaaa", "a", function() while true do end end))"#,
    ] {
        let result = run(script).await;
        assert!(err(&result).contains("CPU budget"), "{script}: {result:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_cannot_be_smuggled_in() {
    // `__gc = false` is still a __gc field; adding one later does not mark
    // the object (Lua 5.4), so it must not run either.
    let result = run("setmetatable({}, { __gc = false })").await;
    assert!(err(&result).contains("__gc"));
    let result = run(
        r#"local mt = {} setmetatable({}, mt) mt.__gc = function() while true do end end
           local junk = {} for i = 1, 20000 do junk[i] = {} end junk = nil
           local s = string.rep("x", 1 << 20) return #s"#,
    )
    .await;
    assert_eq!(ok(&result), &json!(1 << 20));
    // No escape hatches to the raw functions or the shared string metatable.
    let result = run(
        r#"return { debug = debug == nil, string_mt = getmetatable("") == false,
                    method = ("ab"):rep(2),
                    set_string_mt = pcall(setmetatable, "", {}) }"#,
    )
    .await;
    let value = ok(&result);
    assert_eq!(
        value,
        &json!({"debug": true, "string_mt": true, "method": "abab", "set_string_mt": false})
    );
    // A script can't give strings a looping __tostring for the host to run.
    let result = run(
        r#"local mt = getmetatable("") if mt then mt.__tostring = function() while true do end end end
           error("boom")"#,
    )
    .await;
    assert!(err(&result).contains("boom"), "{result:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_prompts_hit_the_memory_limit() {
    let result = run(r#"return agent(string.rep("x", 64 * 1024 * 1024))"#).await;
    assert!(err(&result).contains("memory limit"), "{result:?}");
    // A large but in-limit prompt reaches the child intact.
    let result = run(r#"return agent(string.rep("x", 2 * 1024 * 1024))"#).await;
    assert_eq!(ok(&result), &json!(format!("len:{}", 2 * 1024 * 1024)));
}

#[tokio::test(flavor = "multi_thread")]
async fn fan_out_bombs_are_bounded() {
    // Nested fan-out can't exceed memory or the item cap silently.
    let result = run("local outer = {} for i = 1, 4096 do outer[i] = function()
           local inner = {} for j = 1, 4096 do inner[j] = function() return j end end
           return #parallel(inner) end end
         return #parallel(outer)")
    .await;
    let message = err(&result);
    assert!(
        message.contains("memory") || message.contains("cap"),
        "{message}"
    );
}
