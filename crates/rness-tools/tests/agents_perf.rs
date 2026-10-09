//! WP-6 perf (ignored; prints JSON lines). Sizes from env:
//! - `RNESS_BENCH_AGENTS` = comma list of child counts (default "1,10,50")
//! - `RNESS_BENCH_WF_MEMBERS` = workflow members (default 1000)
//! - `RNESS_BENCH_WF_CONCURRENCY` = workflow concurrency cap (default 16)
//!
//! `cargo test --release -p rness-tools --test agents_perf -- --ignored --nocapture --test-threads=1`

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::session::projection::ModelTurn;
use rness_engine::subagent::{SpawnProvider, SubagentRuntime};
use rness_engine::tools::{ToolCall, ToolRegistry};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_engine::workflow::WorkflowActivity;
use rness_kernel::EventBus;
use rness_protocol::events::StopReason as Stop;
use rness_protocol::events::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Instant answers: measures orchestration, not the model.
struct Instant0;

#[async_trait]
impl Provider for Instant0 {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, request: StepRequest<'_>, _c: &CancellationToken) -> StepOutcome {
        let first = request.context.turns.iter().find_map(|t| match t {
            ModelTurn::User { content } => content.iter().find_map(|p| match p {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        });
        let steps = request
            .context
            .turns
            .iter()
            .filter(|t| matches!(t, ModelTurn::Assistant { .. }))
            .count();
        let mut content = vec![ContentPart::Text { text: "ok".into() }];
        let mut stop = Stop::EndTurn;
        if let Some(script) = first.as_deref().and_then(|f| f.strip_prefix("workflow ")) {
            if steps == 0 {
                stop = Stop::ToolUse;
                content.push(ContentPart::ToolUse {
                    call: "wf".into(),
                    name: "workflow".into(),
                    args: json!({"meta":{"name":"bench","description":"b"},"script":script}),
                });
            }
        }
        StepOutcome::Committed(AssistantMessage {
            model: "fake-1".into(),
            content,
            stop,
            usage: Usage::default(),
            estimated_input: 0,
            chunks: vec![],
        })
    }
}

struct H {
    sessions: Arc<SessionService>,
    tools: Arc<ToolRegistry>,
    _dir: tempfile::TempDir,
}

fn compose(concurrency: usize) -> H {
    let dir = tempfile::tempdir().unwrap();
    let tools = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path().join("sessions")),
        Arc::new(Instant0),
        Arc::clone(&tools),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let runtime = Arc::new(SubagentRuntime::new(Arc::clone(&sessions), 3).with_allow_generic(true));
    runtime.register(Arc::new(SpawnProvider));
    let jobs = rness_tools::jobs::JobRegistry::new();
    jobs.enable_persistence(&dir.path().join("jobs")).unwrap();
    jobs.attach_sessions(&sessions);
    tools.register(Arc::new(rness_tools::jobs::JobOutputTool::new(
        jobs.clone(),
    )));
    tools.register(Arc::new(rness_tools::jobs::JobListTool::new(jobs.clone())));
    tools.register(Arc::new(rness_tools::jobs::JobKillTool::new(jobs.clone())));
    rness_tools::register_subagent(&tools, Arc::clone(&runtime), jobs);
    rness_tools::register_workflow(
        &tools,
        runtime,
        Arc::new(WorkflowActivity::default()),
        rness_tools::workflow::WorkflowConfig {
            limits: rness_engine::workflow::WorkflowLimits {
                max_concurrent_agents: concurrency,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    H {
        sessions,
        tools,
        _dir: dir,
    }
}

fn idle(h: &H, s: &SessionId) -> bool {
    h.sessions.phase(s) == rness_engine::inbox::Phase::Idle
}

fn env_list(name: &str, default: &str) -> Vec<usize> {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

/// Where does per-child spawn time go? Bare store operations, N times.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_spawn_cost_breakdown() {
    let h = compose(16);
    let n = 50;
    let t0 = Instant::now();
    let ids: Vec<_> = (0..n).map(|_| h.sessions.create(None).unwrap()).collect();
    let create = t0.elapsed();
    let t1 = Instant::now();
    for id in &ids {
        h.sessions
            .set_config(id, h.sessions.config(id).unwrap())
            .unwrap();
    }
    let config = t1.elapsed();
    let t2 = Instant::now();
    for _ in 0..n {
        let _ = h.sessions.store().delegations().unwrap();
    }
    let scan = t2.elapsed();
    println!(
        "{}",
        json!({"bench":"wp6.spawn_breakdown","n":n,
               "create_ms_each":create.as_secs_f64()*1e3/n as f64,
               "set_config_ms_each":config.as_secs_f64()*1e3/n as f64,
               "delegations_scan_ms_each":scan.as_secs_f64()*1e3/n as f64})
    );
}

/// Dispatch N background or continuable subagent calls directly (no parent
/// model step), time until all accepted ("spawn") and until all children
/// are idle ("settle").
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_spawn_and_settle_n_children() {
    for mode in ["background", "continuable"] {
        for n in env_list("RNESS_BENCH_AGENTS", "1,10,50") {
            let h = compose(16);
            let parent = h.sessions.create(None).unwrap();
            let calls: Vec<_> = (0..n)
                .map(|i| {
                    let mut args = json!({"provider":"spawn","prompt":format!("c{i}"),"run_in_background":true});
                    if mode == "continuable" {
                        args["background_mode"] = json!("continuable");
                    }
                    ToolCall {
                        call: format!("c{i}"),
                        name: "subagent".into(),
                        args,
                    }
                })
                .collect();
            let t0 = Instant::now();
            let res = h
                .tools
                .dispatch(&parent, &calls, 4, &Default::default())
                .await;
            let spawn = t0.elapsed();
            if let Some(bad) = res.iter().find(|r| r.is_error) {
                panic!("{mode} n={n}: {}", bad.output);
            }
            loop {
                let kids = h.sessions.store().delegated_children(&parent).unwrap();
                if kids.len() == n
                    && kids.iter().all(|(c, _)| {
                        idle(&h, c)
                            && h.sessions
                                .store()
                                .history(c)
                                .unwrap()
                                .iter()
                                .any(|e| matches!(e.event, SessionEvent::TurnEnded { .. }))
                    })
                {
                    break;
                }
                assert!(t0.elapsed() < Duration::from_secs(120));
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            let settle = t0.elapsed();
            println!(
                "{}",
                json!({"bench":"wp6.spawn_settle","mode":mode,"n":n,
                       "spawn_ms":spawn.as_secs_f64()*1e3,"settle_ms":settle.as_secs_f64()*1e3,
                       "per_child_ms":settle.as_secs_f64()*1e3/n as f64})
            );
            h.sessions.begin_teardown(&parent);
        }
    }
}

/// Workflow scheduler throughput at the concurrency cap with trivial
/// members (instant model answers), plus a compute-heavy script to show
/// instruction-hook overhead.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_workflow_throughput() {
    let members = env_list("RNESS_BENCH_WF_MEMBERS", "1000")[0].min(1000);
    let conc = env_list("RNESS_BENCH_WF_CONCURRENCY", "16")[0];
    let cases = [
        (
            "trivial_members",
            format!(
                "local t = {{}} for i = 1, {members} do t[i] = function() return agent('w' .. i) end end \
                 return #compact(parallel(t))"
            ),
        ),
        (
            "compute_script",
            "local s = 0 for i = 1, 20000000 do s = s + i % 7 end return s".to_string(),
        ),
    ];
    for (name, script) in cases {
        let h = compose(conc);
        let parent = h.sessions.create(None).unwrap();
        let t0 = Instant::now();
        h.sessions
            .send(
                &parent,
                UserIntent::Followup,
                vec![ContentPart::Text {
                    text: format!("workflow {script}"),
                }],
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(600), h.sessions.join(&parent))
            .await
            .unwrap();
        let elapsed = t0.elapsed();
        let result = h
            .sessions
            .store()
            .history(&parent)
            .unwrap()
            .into_iter()
            .find_map(|e| match Arc::unwrap_or_clone(e).event {
                SessionEvent::ToolResult(r) if r.name == "workflow" => Some(r),
                _ => None,
            })
            .unwrap();
        let children = h
            .sessions
            .store()
            .delegated_children(&parent)
            .unwrap()
            .len();
        println!(
            "{}",
            json!({"bench":"wp6.workflow","case":name,"concurrency":conc,"children":children,
                   "elapsed_ms":elapsed.as_secs_f64()*1e3,
                   "agents_per_s": if children>0 {children as f64/elapsed.as_secs_f64()} else {0.0},
                   "is_error":result.is_error,"head":result.output.lines().next()})
        );
    }
}
