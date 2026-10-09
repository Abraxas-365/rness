//! WP-6 in-process E2E: subagent fan-out, settle/completion notices exactly
//! once, cancel propagation, depth refusal through the real tool path, job
//! crash recovery (exactly-once across restarts) and the workflow 1000-agent
//! cap. A scripted in-process provider keys its behaviour on each session's
//! FIRST user prompt, so parent and children never share a counter.
//!
//! Sizes: `RNESS_E2E_FANOUT` (default 50).
//!
//! Tests named `bug_*` reproduce confirmed bugs and are `#[ignore]`d so the
//! suite stays green; run them with `--ignored` (see wp-6.md, B6-n).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::session::projection::ModelTurn;
use rness_engine::subagent::{ForkProvider, SpawnProvider, SubagentRuntime};
use rness_engine::tools::{ToolCall, ToolRegistry};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_engine::workflow::WorkflowActivity;
use rness_kernel::EventBus;
use rness_protocol::events::StopReason as Stop;
use rness_protocol::events::*;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

fn fanout() -> usize {
    std::env::var("RNESS_E2E_FANOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
}

// -- scripted provider ------------------------------------------------------

struct Scripted;

fn first_user_text(request: &StepRequest<'_>) -> String {
    request
        .context
        .turns
        .iter()
        .find_map(|t| match t {
            ModelTurn::User { content } => content.iter().find_map(|p| match p {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .unwrap_or_default()
}

fn reply(text: &str, calls: Vec<(String, &str, Value)>) -> StepOutcome {
    let mut content = vec![ContentPart::Text { text: text.into() }];
    let stop = if calls.is_empty() {
        Stop::EndTurn
    } else {
        Stop::ToolUse
    };
    for (call, name, args) in calls {
        content.push(ContentPart::ToolUse {
            call,
            name: name.into(),
            args,
        });
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

fn subagent_calls(n: usize, prompt: &str, mode: &str) -> Vec<(String, &'static str, Value)> {
    (0..n)
        .map(|i| {
            let mut args = json!({"provider":"spawn","prompt":format!("{prompt}-{i}")});
            match mode {
                "background" => args["run_in_background"] = json!(true),
                "continuable" => {
                    args["run_in_background"] = json!(true);
                    args["background_mode"] = json!("continuable");
                }
                _ => {}
            }
            (format!("call-{i}"), "subagent", args)
        })
        .collect()
}

/// Prompt grammar (first user message of the session):
/// - `bg N` / `cont N` / `fg N`: step 0 starts N children (`child-i`) in
///   that mode; later steps end the turn with "ack".
/// - `bgblock N` / `contblock N` / `fgblock N`: same, children `block-i`.
/// - `child-i`: answer `ans-i` after 20 ms.
/// - `block-i`: wait for cancellation.
/// - `nest`: step 0 delegates `nest` in the foreground; then "done".
/// - `workflow <script>`: step 0 calls `workflow` with that script.
/// - `w...`: workflow member, answers "m".
#[async_trait]
impl Provider for Scripted {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        let prompt = first_user_text(&request);
        let step = request
            .context
            .turns
            .iter()
            .filter(|t| matches!(t, ModelTurn::Assistant { .. }))
            .count();
        let (head, rest) = prompt.split_once(' ').unwrap_or((&prompt, ""));
        let n: usize = rest.trim().parse().unwrap_or(0);
        match head {
            "bg" | "cont" | "fg" | "bgblock" | "contblock" | "fgblock" if step == 0 => {
                let mode = match head.trim_end_matches("block") {
                    "bg" => "background",
                    "cont" => "continuable",
                    _ => "foreground",
                };
                let child = if head.ends_with("block") {
                    "block"
                } else {
                    "child"
                };
                reply("", subagent_calls(n, child, mode))
            }
            "nest" if step == 0 => reply(
                "",
                vec![(
                    "call-nest".into(),
                    "subagent",
                    json!({"provider":"spawn","prompt":"nest"}),
                )],
            ),
            "workflow" if step == 0 => reply(
                "",
                vec![(
                    "call-workflow".into(),
                    "workflow",
                    json!({"meta":{"name":"cap-run","description":"e2e"},"script":rest}),
                )],
            ),
            _ if prompt.starts_with("child-") => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                reply(&prompt.replace("child-", "ans-"), vec![])
            }
            _ if prompt.starts_with("block-") => {
                tokio::select! {
                    _ = cancel.cancelled() => StepOutcome::Cancelled { partial: vec![] },
                    _ = tokio::time::sleep(Duration::from_secs(60)) => reply("unblocked", vec![]),
                }
            }
            _ if prompt.starts_with('w') && prompt != "workflow" => reply("m", vec![]),
            _ if step > 0 => reply("ack", vec![]),
            _ => reply("unscripted", vec![]),
        }
    }
}

// -- composition (mirrors rness-cli main.rs wiring) ---------------------------

struct H {
    sessions: Arc<SessionService>,
    tools: Arc<ToolRegistry>,
    jobs: rness_tools::jobs::JobRegistry,
    _dir: tempfile::TempDir,
}

fn compose(
    persist_jobs: bool,
    workflow_limits: Option<rness_engine::workflow::WorkflowLimits>,
) -> H {
    let dir = tempfile::tempdir().unwrap();
    let tools = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path().join("sessions")),
        Arc::new(Scripted),
        Arc::clone(&tools),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let runtime = Arc::new(SubagentRuntime::new(Arc::clone(&sessions), 3).with_allow_generic(true));
    runtime.register(Arc::new(SpawnProvider));
    runtime.register(Arc::new(ForkProvider));
    let jobs = rness_tools::jobs::JobRegistry::new();
    if persist_jobs {
        jobs.enable_persistence(&dir.path().join("jobs")).unwrap();
    }
    jobs.attach_sessions(&sessions);
    tools.register(Arc::new(rness_tools::jobs::JobOutputTool::new(
        jobs.clone(),
    )));
    tools.register(Arc::new(rness_tools::jobs::JobListTool::new(jobs.clone())));
    tools.register(Arc::new(rness_tools::jobs::JobKillTool::new(jobs.clone())));
    rness_tools::register_subagent(&tools, Arc::clone(&runtime), jobs.clone());
    rness_tools::subagent_control::register_subagent_control(&tools, Arc::clone(&runtime));
    if let Some(limits) = workflow_limits {
        rness_tools::register_workflow(
            &tools,
            runtime,
            Arc::new(WorkflowActivity::default()),
            rness_tools::workflow::WorkflowConfig {
                limits,
                ..Default::default()
            },
        );
    }
    H {
        sessions,
        tools,
        jobs,
        _dir: dir,
    }
}

fn send(h: &H, session: &SessionId, text: &str) {
    h.sessions
        .send(
            session,
            UserIntent::Followup,
            vec![ContentPart::Text { text: text.into() }],
        )
        .unwrap();
}

async fn wait_until(what: &str, timeout: Duration, mut pred: impl FnMut() -> bool) {
    let start = Instant::now();
    while !pred() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn history(h: &H, s: &SessionId) -> Vec<SessionEvent> {
    h.sessions
        .store()
        .history(s)
        .unwrap()
        .into_iter()
        .map(|e| Arc::unwrap_or_clone(e).event)
        .collect()
}

fn user_texts(h: &H, s: &SessionId) -> Vec<String> {
    history(h, s)
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::UserMessage(m) => Some(
                m.content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}

fn turn_outcomes(h: &H, s: &SessionId) -> Vec<TurnOutcome> {
    history(h, s)
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::TurnEnded { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect()
}

fn tool_results(h: &H, s: &SessionId) -> Vec<ToolResult> {
    history(h, s)
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect()
}

/// Job ids mentioned in completion notices, with the count per id.
fn job_notice_counts(h: &H, s: &SessionId) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for text in user_texts(h, s) {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Job ") {
                if let Some((id, _)) = rest.split_once(':') {
                    *out.entry(id.to_string()).or_default() += 1;
                }
            }
        }
    }
    out
}

/// Child ids mentioned in continuable settle notices, with counts.
fn settle_notice_counts(h: &H, s: &SessionId) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for text in user_texts(h, s) {
        for part in text.split("[subagent ").skip(1) {
            if let Some((id, _)) = part.split_once(" settled") {
                *out.entry(id.to_string()).or_default() += 1;
            }
        }
    }
    out
}

fn children_of(h: &H, s: &SessionId) -> Vec<(SessionId, rness_protocol::branch::Delegation)> {
    h.sessions.store().delegated_children(s).unwrap()
}

fn idle(h: &H, s: &SessionId) -> bool {
    h.sessions.phase(s) == rness_engine::inbox::Phase::Idle
}

fn job_ids_from_results(h: &H, parent: &SessionId) -> Vec<String> {
    tool_results(h, parent)
        .iter()
        .filter(|r| r.name == "subagent")
        .filter_map(|r| {
            r.output
                .split("as job ")
                .nth(1)
                .and_then(|t| t.split_whitespace().next())
                .map(str::to_owned)
        })
        .collect()
}

// -- 1. fan-out ------------------------------------------------------------------

async fn background_fanout(persist: bool) {
    let n = fanout();
    let h = compose(persist, None);
    let parent = h.sessions.create(None).unwrap();
    let t0 = Instant::now();
    send(&h, &parent, &format!("bg {n}"));
    // All N notices arrive and the parent drains to idle.
    wait_until("all job notices", Duration::from_secs(60), || {
        job_notice_counts(&h, &parent).len() >= n && idle(&h, &parent)
    })
    .await;
    let settled = t0.elapsed();
    // Linger past the durable poller period (1 s) to catch duplicates.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    wait_until("parent idle", Duration::from_secs(10), || idle(&h, &parent)).await;

    let started = job_ids_from_results(&h, &parent);
    assert_eq!(started.len(), n, "every call launched a job");
    let counts = job_notice_counts(&h, &parent);
    let dup: Vec<_> = counts.iter().filter(|(_, c)| **c != 1).collect();
    assert!(dup.is_empty(), "duplicate job notices: {dup:?}");
    for id in &started {
        assert_eq!(counts.get(id), Some(&1), "notice for {id}");
    }
    // Children are ordinary sessions in the same store, depth 1, one-shot.
    let kids = children_of(&h, &parent);
    assert_eq!(kids.len(), n);
    for (child, d) in &kids {
        assert_eq!(d.depth, 1);
        assert_eq!(d.mode, rness_protocol::branch::DelegationMode::OneShot);
        assert_eq!(turn_outcomes(&h, child), vec![TurnOutcome::Completed]);
    }
    assert_eq!(h.sessions.list().unwrap().len(), n + 1);
    // Every parent turn completed; no notice was lost to a failed turn.
    assert!(turn_outcomes(&h, &parent)
        .iter()
        .all(|o| *o == TurnOutcome::Completed));
    println!(
        "{}",
        json!({"bench":"wp6.bg_fanout","persist":persist,"n":n,"settle_ms":settled.as_millis(),
               "parent_turns":turn_outcomes(&h, &parent).len()})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn background_fanout_notices_exactly_once_in_memory_jobs() {
    background_fanout(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_fanout_notices_exactly_once_persistent_jobs() {
    background_fanout(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn continuable_fanout_settles_exactly_once_each() {
    let n = fanout();
    let h = compose(true, None);
    let parent = h.sessions.create(None).unwrap();
    let t0 = Instant::now();
    send(&h, &parent, &format!("cont {n}"));
    wait_until("all settle notices", Duration::from_secs(60), || {
        settle_notice_counts(&h, &parent).len() >= n && idle(&h, &parent)
    })
    .await;
    let settled = t0.elapsed();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let kids = children_of(&h, &parent);
    assert_eq!(kids.len(), n);
    let counts = settle_notice_counts(&h, &parent);
    for (child, d) in &kids {
        assert_eq!(d.mode, rness_protocol::branch::DelegationMode::Continuable);
        assert_eq!(counts.get(child), Some(&1), "settle notice for {child}");
        assert!(idle(&h, child));
    }
    assert_eq!(counts.len(), n, "{counts:?}");
    // Continuable children never create jobs.
    assert!(job_notice_counts(&h, &parent).is_empty());
    assert_eq!(h.jobs.count(&parent), 0);
    println!(
        "{}",
        json!({"bench":"wp6.cont_fanout","n":n,"settle_ms":settled.as_millis(),
               "parent_turns":turn_outcomes(&h, &parent).len()})
    );
}

// -- 2. cancel propagation -------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn parent_cancel_cancels_foreground_child() {
    let h = compose(true, None);
    let parent = h.sessions.create(None).unwrap();
    send(&h, &parent, "fgblock 1");
    wait_until("child running", Duration::from_secs(10), || {
        children_of(&h, &parent)
            .first()
            .is_some_and(|(c, _)| !idle(&h, c))
    })
    .await;
    let child = children_of(&h, &parent)[0].0.clone();
    h.sessions.cancel(&parent);
    tokio::time::timeout(Duration::from_secs(10), h.sessions.join(&parent))
        .await
        .expect("parent settles after cancel");
    wait_until("child idle", Duration::from_secs(10), || idle(&h, &child)).await;
    assert_eq!(turn_outcomes(&h, &child), vec![TurnOutcome::Cancelled]);
    assert_eq!(turn_outcomes(&h, &parent), vec![TurnOutcome::Cancelled]);
}

/// Background one-shot children outlive the parent's turn by design; the
/// documented way to stop one is `job_kill`. It must cancel the child.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "B6-1: job_kill on a background subagent job never cancels the child"]
async fn bug_job_kill_cancels_background_subagent_child() {
    let h = compose(true, None);
    let parent = h.sessions.create(None).unwrap();
    send(&h, &parent, "bgblock 1");
    wait_until("child running", Duration::from_secs(10), || {
        children_of(&h, &parent)
            .first()
            .is_some_and(|(c, _)| !idle(&h, c))
            && idle(&h, &parent)
    })
    .await;
    let child = children_of(&h, &parent)[0].0.clone();
    let job = job_ids_from_results(&h, &parent)[0].clone();
    let res = h
        .tools
        .dispatch(
            &parent,
            &[ToolCall {
                call: "kill".into(),
                name: "job_kill".into(),
                args: json!({"job_id": job}),
            }],
            1,
            &Default::default(),
        )
        .await;
    assert!(
        res[0].output.contains("cancellation requested"),
        "{}",
        res[0].output
    );
    // Expectation: the child turn is cancelled within a few seconds.
    let start = Instant::now();
    while !idle(&h, &child) && start.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        idle(&h, &child),
        "child still running {:?} after job_kill; job snapshot: {:?}",
        start.elapsed(),
        h.jobs
            .list(&parent)
            .iter()
            .map(|j| (&j.job_id, &j.status, j.running))
            .collect::<Vec<_>>()
    );
    assert_eq!(turn_outcomes(&h, &child), vec![TurnOutcome::Cancelled]);
}

/// Observation (not a contract): what parent teardown does to running
/// background and continuable children. Prints JSON; asserts only that
/// teardown itself completes.
#[tokio::test(flavor = "multi_thread")]
async fn teardown_of_parent_with_running_children_observation() {
    let h = compose(true, None);
    let parent = h.sessions.create(None).unwrap();
    send(&h, &parent, "bgblock 3");
    wait_until("bg children running", Duration::from_secs(10), || {
        children_of(&h, &parent).len() == 3 && idle(&h, &parent)
    })
    .await;
    send(&h, &parent, "contblock 3");
    // Second prompt: first user text is still "bgblock 3", so it just acks.
    // Start continuable children through dispatch instead.
    h.sessions.join(&parent).await;
    let res = h
        .tools
        .dispatch(
            &parent,
            &subagent_calls(3, "block", "continuable")
                .into_iter()
                .map(|(call, name, args)| ToolCall {
                    call,
                    name: name.into(),
                    args,
                })
                .collect::<Vec<_>>(),
            1,
            &Default::default(),
        )
        .await;
    assert!(res.iter().all(|r| !r.is_error));
    wait_until("6 children", Duration::from_secs(10), || {
        children_of(&h, &parent).len() == 6
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.sessions.begin_teardown(&parent);
    tokio::time::timeout(Duration::from_secs(10), h.sessions.join(&parent))
        .await
        .expect("teardown join");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut still_running = HashMap::<String, usize>::new();
    for (child, d) in children_of(&h, &parent) {
        if !idle(&h, &child) {
            *still_running.entry(format!("{:?}", d.mode)).or_default() += 1;
        }
    }
    println!(
        "{}",
        json!({"observation":"wp6.teardown_children_running_after_2s","running":still_running})
    );
}

// -- 3. depth through the tool path ---------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn nested_delegation_is_refused_cleanly_at_max_depth() {
    let h = compose(true, None);
    let root = h.sessions.create(None).unwrap();
    send(&h, &root, "nest");
    tokio::time::timeout(Duration::from_secs(20), h.sessions.join(&root))
        .await
        .expect("chain settles");
    let mut chain = vec![root.clone()];
    while let Some((child, d)) = children_of(&h, chain.last().unwrap()).into_iter().next() {
        assert_eq!(d.depth as usize, chain.len());
        chain.push(child);
    }
    assert_eq!(chain.len(), 4, "root + depth 1..=3");
    let deepest = chain.last().unwrap();
    let refused = tool_results(&h, deepest);
    assert_eq!(refused.len(), 1);
    assert!(refused[0].is_error, "{}", refused[0].output);
    assert!(refused[0].output.contains("depth"), "{}", refused[0].output);
    for s in &chain {
        assert_eq!(turn_outcomes(&h, s), vec![TurnOutcome::Completed], "{s}");
    }
    // The refusal created no orphan session.
    assert_eq!(h.sessions.list().unwrap().len(), 4);
}

// -- 4. job crash recovery: exactly once across restarts ---------------------------

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_jobs_are_recovered_and_noticed_exactly_once_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let jobs_root = dir.path().join("jobs");
    let make_sessions = || {
        Arc::new(SessionService::new(
            SessionStore::new(dir.path().join("sessions")),
            Arc::new(Scripted),
            Arc::new(ToolRegistry::default()),
            TurnConfig::default(),
            Arc::new(EventBus::default()),
        ))
    };
    // Process 1: owner session with 5 running jobs and 1 settled, then "crash".
    let sessions = make_sessions();
    let parent = sessions.create(None).unwrap();
    let mut running = Vec::new();
    {
        let jobs = rness_tools::jobs::JobRegistry::new();
        jobs.enable_persistence(&jobs_root).unwrap();
        // Not attached: simulates a crash before the poller ran.
        for i in 0..5 {
            let (id, writer) = jobs.start_owned("bash", format!("job {i}"), Some(&parent));
            writer.append(b"partial");
            running.push(id);
            std::mem::forget(writer); // never settles: process died
        }
        let (id, writer) = jobs.start_owned("bash", "done".into(), Some(&parent));
        writer.settle(rness_tools::jobs::JobStatus::Exited(Some(0)));
        running.push(id);
        // `forget` keeps the owner lock alive inside the leaked writers'
        // Arc<Job>, but the lock lives in the registry inner; drop it.
        drop(jobs);
    }
    drop(sessions);

    let count_notices = |s: &Arc<SessionService>| {
        let mut per: HashMap<String, usize> = HashMap::new();
        for e in s.store().history(&parent).unwrap() {
            if let SessionEvent::UserMessage(m) = &e.event {
                for p in &m.content {
                    if let ContentPart::Text { text } = p {
                        if let Some(rest) = text.strip_prefix("Job ") {
                            let id = rest.split(':').next().unwrap().to_string();
                            *per.entry(id).or_default() += 1;
                        }
                    }
                }
            }
        }
        per
    };

    // Processes 2 and 3: restart twice; notices must not repeat.
    for round in 0..2 {
        let sessions = make_sessions();
        let jobs = rness_tools::jobs::JobRegistry::new();
        jobs.enable_persistence(&jobs_root).unwrap();
        jobs.wait_recovery();
        jobs.attach_sessions(&sessions);
        let start = Instant::now();
        while count_notices(&sessions).len() < running.len()
            && start.elapsed() < Duration::from_secs(10)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Let the poller run a few more periods.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        sessions.join(&parent).await;
        let per = count_notices(&sessions);
        assert_eq!(per.len(), running.len(), "round {round}: {per:?}");
        assert!(
            per.values().all(|c| *c == 1),
            "round {round}: duplicate notices {per:?}"
        );
        let snaps = jobs.list(&parent);
        let interrupted = snaps
            .iter()
            .filter(|j| j.status.contains("interrupted"))
            .count();
        assert_eq!(interrupted, 5, "round {round}: {snaps:?}");
        drop(jobs);
        drop(sessions);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// -- 5. workflow 1000-agent cap --------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn workflow_total_agent_cap_is_fatal_at_1000_with_real_children() {
    let limits = rness_engine::workflow::WorkflowLimits {
        max_concurrent_agents: 16,
        ..Default::default()
    };
    assert_eq!(limits.max_total_agents, 1000);
    let h = compose(true, Some(limits));
    let parent = h.sessions.create(None).unwrap();
    let script =
        "local t = {} for i = 1, 1001 do t[i] = function() return agent('w' .. i) end end \
                  local r = parallel(t) return #compact(r)";
    let t0 = Instant::now();
    send(&h, &parent, &format!("workflow {script}"));
    tokio::time::timeout(Duration::from_secs(300), h.sessions.join(&parent))
        .await
        .expect("workflow settles");
    let elapsed = t0.elapsed();
    let result = tool_results(&h, &parent)
        .into_iter()
        .find(|r| r.name == "workflow")
        .expect("workflow result");
    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("1000") || result.output.contains("cap"),
        "{}",
        result.output
    );
    let kids = children_of(&h, &parent).len();
    assert!(kids <= 1000, "children {kids}");
    // No child is left running after the fatal stop.
    let running = children_of(&h, &parent)
        .iter()
        .filter(|(c, _)| !idle(&h, c))
        .count();
    assert_eq!(running, 0);
    println!(
        "{}",
        json!({"bench":"wp6.workflow_cap","children":kids,"elapsed_ms":elapsed.as_millis(),
               "error":result.output.lines().next()})
    );
}
