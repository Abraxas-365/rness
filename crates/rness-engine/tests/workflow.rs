//! Workflow engine tests with a fake child runner (no model, no sessions).
//!
//! Fake protocol, keyed by prompt:
//! - `fail` → child failure; `fatal` → infrastructure failure;
//! - `hang` → waits for cancellation; `slow` → 40 ms then completes;
//! - `after <p>` → waits until a child with prompt `<p>` has started;
//! - `nostruct` → completes without structured output;
//! - anything else → completes with `re:<prompt>` (or `{"seq": n}` when a
//!   schema was requested).

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rness_engine::workflow::{
    self, AgentOutcome, ChildOutcome, ChildRequest, ChildRunner, WorkflowEvent, WorkflowLimits,
    WorkflowResult, WorkflowStop,
};
use rness_protocol::events::SessionId;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Fake {
    started: tokio::sync::watch::Sender<HashSet<String>>,
    live: AtomicUsize,
    peak: AtomicUsize,
    cancelled: AtomicUsize,
    requests: Mutex<Vec<ChildRequest>>,
}

#[async_trait]
impl ChildRunner for Fake {
    fn check_role(&self, role: Option<&str>) -> Result<(), String> {
        match role {
            Some("bad") => Err("unknown agent 'bad'".into()),
            _ => Ok(()),
        }
    }

    async fn run(
        &self,
        request: ChildRequest,
        cancel: CancellationToken,
        started: Box<dyn FnOnce(SessionId) + Send>,
    ) -> ChildOutcome {
        started(format!("child-{}", request.seq));
        self.requests.lock().unwrap().push(request.clone());
        self.started.send_modify(|s| {
            s.insert(request.prompt.clone());
        });
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let outcome = self.behave(&request, &cancel).await;
        self.live.fetch_sub(1, Ordering::SeqCst);
        outcome
    }
}

impl Fake {
    async fn behave(&self, request: &ChildRequest, cancel: &CancellationToken) -> ChildOutcome {
        let prompt = request.prompt.as_str();
        if let Some(other) = prompt.strip_prefix("after ") {
            let mut rx = self.started.subscribe();
            tokio::select! {
                _ = rx.wait_for(|s| s.contains(other)) => {}
                _ = cancel.cancelled() => return ChildOutcome::Cancelled,
            }
        }
        match prompt {
            "fail" => ChildOutcome::Failed("child failed".into()),
            "fatal" => ChildOutcome::Fatal("provider exploded".into()),
            "hang" => {
                cancel.cancelled().await;
                self.cancelled.fetch_add(1, Ordering::SeqCst);
                ChildOutcome::Cancelled
            }
            "nostruct" => ChildOutcome::Completed {
                text: "prose".into(),
                structured: None,
            },
            _ => {
                if prompt == "slow" {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                ChildOutcome::Completed {
                    text: format!("re:{prompt}"),
                    structured: request.schema.as_ref().map(|_| json!({"seq": request.seq})),
                }
            }
        }
    }
}

fn meta() -> workflow::WorkflowMeta {
    workflow::validate_meta(&json!({"name":"test-run","description":"test"})).unwrap()
}

struct Outcome {
    result: WorkflowResult,
    events: Vec<WorkflowEvent>,
    fake: Arc<Fake>,
}

async fn run_with(
    script: &str,
    args: Value,
    limits: WorkflowLimits,
    cancel: CancellationToken,
) -> Outcome {
    let fake = Arc::new(Fake::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        workflow::run(
            meta(),
            script.into(),
            args,
            limits,
            fake.clone(),
            Arc::new(move |e| sink.lock().unwrap().push(e)),
            cancel,
        ),
    )
    .await
    .expect("workflow run must settle");
    let events = events.lock().unwrap().clone();
    Outcome {
        result,
        events,
        fake,
    }
}

fn limits() -> WorkflowLimits {
    WorkflowLimits {
        max_concurrent_agents: 8,
        dispose_grace: Duration::from_secs(2),
        ..WorkflowLimits::default()
    }
}

async fn run(script: &str) -> Outcome {
    run_with(script, Value::Null, limits(), CancellationToken::new()).await
}

fn value(outcome: &Outcome) -> Value {
    assert_eq!(
        outcome.result.stop,
        WorkflowStop::Completed,
        "error: {:?}",
        outcome.result.error
    );
    outcome.result.value.clone().unwrap()
}

fn error(outcome: &Outcome) -> String {
    assert_eq!(
        outcome.result.stop,
        WorkflowStop::Error,
        "value: {:?}",
        outcome.result.value
    );
    outcome.result.error.clone().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sequential_agents_return_child_text() {
    let out = run(r#"local a = agent("x"); local b = agent("y"); return {a, b}"#).await;
    assert_eq!(value(&out), json!(["re:x", "re:y"]));
    assert_eq!(out.result.agents_started, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_keeps_input_order_and_runs_concurrently() {
    // Item 1 cannot finish until item 2 has started: sequential
    // execution would deadlock (and time out).
    let out = run(r#"return parallel({
             function() return agent("after y") end,
             function() return agent("y") end,
           })"#)
    .await;
    assert_eq!(value(&out), json!(["re:after y", "re:y"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn pipeline_has_no_barrier_between_stages() {
    // B's stage 1 waits for A's stage 2 to START: a stage barrier would
    // deadlock.
    let out = run(r#"return pipeline({"A", "B"},
             function(_, item)
               if item == "B" then return agent("after s2 A") end
               return agent("s1 " .. item)
             end,
             function(prev, item) return agent("s2 " .. item) end)"#)
    .await;
    assert_eq!(value(&out), json!(["re:s2 A", "re:s2 B"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn pipeline_stage_signature_matches_dsh() {
    // Stage 1 receives the item as `prev` (dsh: `let value = item`).
    let out = run(r#"return pipeline({"a", "b"},
             function(prev, item, index) return prev .. item .. index end,
             function(prev) return prev .. "!" end)"#)
    .await;
    assert_eq!(value(&out), json!(["aa1!", "bb2!"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_and_ordinary_errors_become_nil_holes() {
    let out = run(r#"local r = parallel({
             function() return agent("fail") end,
             function() return agent("ok") end,
             function() error("boom") end,
           })
           local p = pipeline({"x", "y"},
             function(prev, item) if item == "x" then error("stage") end return item end,
             function(prev) return agent("second " .. prev) end)
           return { r = r, c = compact(r), p = p }"#)
    .await;
    assert_eq!(
        value(&out),
        json!({"r": [null, "re:ok", null], "c": ["re:ok"], "p": [null, "re:second y"]})
    );
    // The failed pipeline item skipped its remaining stage.
    assert_eq!(out.result.agents_started, 3);
    assert!(out.events.contains(&WorkflowEvent::AgentEnded {
        seq: 1,
        outcome: AgentOutcome::Failed
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn misuse_is_fatal_even_inside_combinators_and_pcall() {
    let cases = [
        (
            r#"return parallel({ function() return agent("x", { bogus = 1 }) end })"#,
            r#"option "bogus" is not recognized"#,
        ),
        (
            r#"return pipeline({1}, function() return agent("x", { effort = "high" }) end)"#,
            r#"option "effort" is deferred"#,
        ),
        (
            r#"return pcall(agent, "x", { schema = { type = "object", properties = { a = { type = "string", pattern = "x" } } } })"#,
            "outside the supported subset",
        ),
        (
            r#"return agent("x", { role = "bad" })"#,
            "unknown agent 'bad'",
        ),
        (
            r#"return agent("x", { provider = "cloud" })"#,
            "must be \"spawn\" or \"fork\"",
        ),
        (r#"return agent("")"#, "non-empty prompt"),
        (r#"return parallel({ 1 })"#, "item 1 is not a function"),
        (r#"return pipeline({ 1 })"#, "at least one stage"),
        (r#"phase(3)"#, "non-empty title"),
        (r#"return agent("fatal")"#, "provider exploded"),
        (r#"return function() end"#, "not plain JSON data"),
        (
            r#"return { 1, x = 2 }"#,
            "mixes array items and named fields",
        ),
        (r#"local t = {}; t.self = t; return t"#, "cycle"),
        (
            r#"error("top")"#,
            "workflow script failed: workflow:test-run:1: top",
        ),
    ];
    for (script, expected) in cases {
        let out = run(script).await;
        let message = error(&out);
        assert!(message.contains(expected), "{script}: {message}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_yields_through_pcall() {
    let out = run(r#"local ok, v = pcall(agent, "x")
           local ok2, e = pcall(error, "caught")
           return { ok = ok, v = v, ok2 = ok2 }"#)
    .await;
    assert_eq!(value(&out), json!({"ok": true, "v": "re:x", "ok2": false}));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrency_cap_is_respected() {
    let out = run_with(
        r#"local t = {}
           for i = 1, 6 do t[i] = function() return agent("slow") end end
           return compact(parallel(t))"#,
        Value::Null,
        WorkflowLimits {
            max_concurrent_agents: 2,
            ..limits()
        },
        CancellationToken::new(),
    )
    .await;
    assert_eq!(value(&out).as_array().unwrap().len(), 6);
    assert_eq!(out.fake.peak.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn total_agent_and_item_caps_are_fatal() {
    let capped = WorkflowLimits {
        max_total_agents: 3,
        max_items_per_call: 2,
        ..limits()
    };
    let out = run_with(
        r#"for i = 1, 5 do agent("x") end"#,
        Value::Null,
        capped.clone(),
        CancellationToken::new(),
    )
    .await;
    assert!(error(&out).contains("total agent cap (3)"));
    assert_eq!(out.result.agents_started, 3);
    let out = run_with(
        r#"return pipeline({1, 2, 3}, function(p) return p end)"#,
        Value::Null,
        capped,
        CancellationToken::new(),
    )
    .await;
    assert!(error(&out).contains("received 3 items"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cpu_budget_stops_busy_loops_and_cannot_be_caught() {
    let budget = WorkflowLimits {
        script_budget: Duration::from_millis(150),
        ..limits()
    };
    for script in [
        "while true do end",
        "pcall(function() while true do end end) return 1",
        "local ok = pcall(function() while true do end end) while true do end",
    ] {
        let out = run_with(
            script,
            Value::Null,
            budget.clone(),
            CancellationToken::new(),
        )
        .await;
        assert!(error(&out).contains("CPU budget"), "{script}");
    }
    // Waiting on children does not count against the budget.
    let out = run_with(
        r#"for i = 1, 5 do agent("slow") end return "done""#,
        Value::Null,
        WorkflowLimits {
            script_budget: Duration::from_millis(20),
            ..limits()
        },
        CancellationToken::new(),
    )
    .await;
    assert_eq!(value(&out), json!("done"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_mid_run_cancels_in_flight_children() {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        trigger.cancel();
    });
    let out = run_with(
        r#"return parallel({ function() return agent("hang") end, function() return agent("hang") end })"#,
        Value::Null,
        limits(),
        cancel,
    )
    .await;
    assert_eq!(out.result.stop, WorkflowStop::Cancelled);
    assert_eq!(out.fake.cancelled.load(Ordering::SeqCst), 2);
    assert_eq!(out.result.agents_started, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_interrupts_a_busy_script() {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let out = run_with("while true do end", Value::Null, limits(), cancel).await;
    assert_eq!(out.result.stop, WorkflowStop::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_returns_the_structured_value() {
    let out = run(
        r#"local s = { type = "object", required = { "seq" }, properties = { seq = { type = "integer" } } }
           local a = agent("x", { schema = s })
           local b = agent("nostruct", { schema = s })
           return { seq = a.seq, missing = b == nil }"#,
    )
    .await;
    assert_eq!(value(&out), json!({"seq": 1, "missing": true}));
    let requests = out.fake.requests.lock().unwrap();
    assert_eq!(
        requests[0].schema.as_ref().unwrap()["required"],
        json!(["seq"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_lua_tables_in_schemas_are_objects() {
    let out = run(r#"return agent("x", { schema = { type = "object", properties = {} } })"#).await;
    assert_eq!(value(&out), json!({"seq": 1}));
}

#[tokio::test(flavor = "multi_thread")]
async fn args_json_and_sandbox() {
    let out = run_with(
        r#"return {
             crates = args.crates,
             encoded = json.encode({ 1, 2 }),
             decoded = json.decode('{"k":[],"o":{}}'),
             no_coroutine = coroutine == nil,
             no_load = load == nil and require == nil and io == nil and os == nil,
           }"#,
        json!({"crates": ["a", "b"]}),
        limits(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        value(&out),
        json!({
            "crates": ["a", "b"],
            "encoded": "[1,2]",
            "decoded": {"k": [], "o": {}},
            "no_coroutine": true,
            "no_load": true,
        })
    );
    let out = run_with("return 1", json!([1]), limits(), CancellationToken::new()).await;
    assert!(error(&out).contains("args must be a JSON object"));
}

#[tokio::test(flavor = "multi_thread")]
async fn phases_labels_and_log_reach_the_observer() {
    let out = run(r#"phase("Scan")
           log("starting")
           agent("first line\nsecond line")
           agent("y", { label = "custom", phase = "Verify" })"#)
    .await;
    assert_eq!(value(&out), Value::Null);
    let queued: Vec<_> = out
        .events
        .iter()
        .filter_map(|e| match e {
            WorkflowEvent::AgentQueued { seq, label, phase } => {
                Some((*seq, label.clone(), phase.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        queued,
        vec![
            (1, "first line".to_string(), Some("Scan".to_string())),
            (2, "custom".to_string(), Some("Verify".to_string())),
        ]
    );
    assert_eq!(out.events[0], WorkflowEvent::Phase("Scan".into()));
    assert_eq!(out.events[1], WorkflowEvent::Log("starting".into()));
    assert!(out.events.contains(&WorkflowEvent::AgentStarted {
        seq: 1,
        child: "child-1".into()
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_limit_is_fatal() {
    let out = run_with(
        r#"local t = {} for i = 1, 1e9 do t[i] = string.rep("x", 1024) .. i end"#,
        Value::Null,
        WorkflowLimits {
            memory_bytes: 8 * 1024 * 1024,
            ..limits()
        },
        CancellationToken::new(),
    )
    .await;
    assert!(error(&out).contains("memory limit"));
    // Like the CPU budget, a caught memory error is re-raised (sticky).
    let out = run_with(
        r#"local ok = pcall(string.rep, "x", 1e9) return { caught = ok }"#,
        Value::Null,
        WorkflowLimits {
            memory_bytes: 8 * 1024 * 1024,
            ..limits()
        },
        CancellationToken::new(),
    )
    .await;
    assert!(error(&out).contains("memory limit"));
}

#[tokio::test(flavor = "multi_thread")]
async fn xpcall_handlers_cannot_swallow_a_memory_error() {
    // A Rust callback's allocation failure reaches the xpcall handler; the
    // handler returning a value must not make the run continue.
    let tight = || WorkflowLimits {
        memory_bytes: 4 * 1024 * 1024,
        ..limits()
    };
    // Control: the input alone fits, so the failure below is json.decode's.
    let control = run_with(
        r#"local big = "[" .. string.rep('[1,2,3,4,5,6,7,8],', 60000) .. '"x"]'
           return #big"#,
        Value::Null,
        tight(),
        CancellationToken::new(),
    )
    .await;
    assert!(value(&control).as_u64().unwrap() > 1_000_000);
    let out = run_with(
        r#"local big = "[" .. string.rep('[1,2,3,4,5,6,7,8],', 60000) .. '"x"]'
           local ok = xpcall(function() return json.decode(big) end, function() return 1 end)
           return { caught = ok }"#,
        Value::Null,
        tight(),
        CancellationToken::new(),
    )
    .await;
    assert!(error(&out).contains("memory limit"), "{:?}", out.result);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nil_stage_result_skips_the_remaining_stages() {
    let out = run(r#"local calls = 0
           local p = pipeline({"fail", "ok"},
             function(prev) return agent(prev) end,
             function(_, item) calls = calls + 1 return agent("second " .. item) end)
           return { p = p, calls = calls }"#)
    .await;
    assert_eq!(
        value(&out),
        json!({"p": [null, "re:second ok"], "calls": 1})
    );
    assert_eq!(out.result.agents_started, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_finalizers_are_rejected() {
    // Finalizers run with hooks disabled: an endless one would escape the
    // budget and cancellation, so scripts cannot install any.
    let out =
        run(r#"setmetatable({}, { __gc = function() while true do end end }) return 1"#).await;
    assert!(error(&out).contains("__gc"), "{:?}", out.result.error);
    let out =
        run(r#"local t = setmetatable({}, { __index = function() return 7 end }) return t.x"#)
            .await;
    assert_eq!(value(&out), json!(7));
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_subtrees_cannot_expand_without_bound() {
    for script in [
        "local t = {} for i = 1, 60 do t = {t, t} end return t",
        "local t = {} for i = 1, 60 do t = {t, t} end return json.encode(t)",
        "local t = {} for i = 1, 60 do t = {t, t} end return agent('x', { schema = { type = 'object', properties = { a = t } } })",
    ] {
        let out = run(script).await;
        assert!(error(&out).contains("too large"), "{script}: {:?}", out.result.error);
    }
}

#[test]
fn meta_and_script_are_validated_before_running() {
    assert!(workflow::validate_meta(
        &json!({"name":"a-b","description":"d","phases":[{"title":"T","detail":"x"}]})
    )
    .is_ok());
    let err = workflow::validate_meta(&json!({"name":"Bad Name","description":"","extra":1}))
        .unwrap_err();
    assert!(err.contains("kebab-case"), "{err}");
    assert!(err.contains("meta.description"), "{err}");
    assert!(
        err.contains("meta.extra is not a recognized field"),
        "{err}"
    );
    let limits = WorkflowLimits::default();
    let err = workflow::check_script(&meta(), "return (", &limits).unwrap_err();
    assert!(err.contains("does not parse"), "{err}");
    assert!(err.contains("workflow:test-run"), "{err}");
    let big = "-".repeat(limits.max_script_bytes + 1);
    assert!(workflow::check_script(&meta(), &big, &limits)
        .unwrap_err()
        .contains("byte limit"));
}
