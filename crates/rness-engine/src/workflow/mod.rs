//! Workflow engine: run a model-written Lua orchestration script that
//! starts subagents (dsh `packages/workflow`, in Lua instead of JS).
//!
//! One fresh sandboxed VM per run, on its own blocking thread. The script
//! body is the root coroutine; `agent()` yields to a Rust scheduler that
//! starts children on the async side, so `parallel`/`pipeline` get real
//! concurrency without threads in the VM. Only the script's return value
//! leaves the run — child transcripts never reach the caller.
//!
//! Error model (dsh): a child that fails resolves its item to `nil`;
//! misuse (bad arguments, unknown options, unsupported schema, tripped
//! caps, start failures, cancellation) is FATAL. Fatal errors are sticky
//! here: once raised, the run ends at the next suspension even if the
//! script caught the error with `pcall` (stricter than dsh, where a caught
//! fatal only re-raises through the combinators).

mod activity;
mod children;
mod convert;
mod scheduler;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How far past its budget a Lua slice may run (stuck in a C call, where
/// the instruction hook cannot fire) before the worker is detached.
const STUCK_GRACE: Duration = Duration::from_secs(2);

use async_trait::async_trait;
use rness_protocol::events::SessionId;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use activity::WorkflowActivity;
pub use children::SubagentChildren;

/// The model-facing tool's name. Hidden in `ptc` exposure, refused inside
/// `run_code` programs, and withheld from workflow members.
pub const TOOL: &str = "workflow";

/// Validated `meta` block: data only, never evaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowMeta {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub phases: Vec<MetaPhase>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetaPhase {
    pub title: String,
    pub detail: Option<String>,
}

/// Per-run resource caps (dsh defaults).
#[derive(Debug, Clone)]
pub struct WorkflowLimits {
    /// Children in flight at once; later `agent()` calls queue FIFO.
    pub max_concurrent_agents: usize,
    /// Total `agent()` calls per run (runaway-loop backstop).
    pub max_total_agents: usize,
    /// Items per `parallel`/`pipeline` call.
    pub max_items_per_call: usize,
    /// Budget for the script's own Lua execution, accumulated across the
    /// whole run (wall time spent inside Lua resumes, a CPU-time proxy).
    /// Time spent waiting on children is not counted.
    pub script_budget: Duration,
    /// VM memory limit in bytes.
    pub memory_bytes: usize,
    /// Script source size limit in bytes.
    pub max_script_bytes: usize,
    /// How long cancelled children get to settle before the run returns.
    pub dispose_grace: Duration,
}

impl Default for WorkflowLimits {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            max_concurrent_agents: cores.saturating_sub(2).clamp(1, 16),
            max_total_agents: 1000,
            max_items_per_call: 4096,
            script_budget: Duration::from_millis(5000),
            memory_bytes: 64 * 1024 * 1024,
            max_script_bytes: 64 * 1024,
            dispose_grace: Duration::from_millis(5000),
        }
    }
}

/// How a member child ended, as the script sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOutcome {
    Completed,
    Failed,
    Cancelled,
}

/// Progress for observers (the live card). Delivered in order, from the
/// run's own thread.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowEvent {
    Phase(String),
    Log(String),
    /// `agent()` accepted; the child may still wait for a slot.
    AgentQueued {
        seq: usize,
        label: String,
        phase: Option<String>,
    },
    /// The child session exists.
    AgentStarted {
        seq: usize,
        child: SessionId,
    },
    AgentEnded {
        seq: usize,
        outcome: AgentOutcome,
    },
}

pub type Observer = Arc<dyn Fn(WorkflowEvent) + Send + Sync>;

/// One `agent()` call, validated.
#[derive(Debug, Clone)]
pub struct ChildRequest {
    pub seq: usize,
    pub prompt: String,
    pub label: String,
    pub phase: Option<String>,
    /// rness agent role (`scout`, `reviewer`, …).
    pub role: Option<String>,
    /// Child provider: `spawn` or `fork`.
    pub provider: String,
    pub schema: Option<Value>,
}

/// A settled child.
#[derive(Debug, Clone)]
pub enum ChildOutcome {
    /// Completed. `structured` is the captured value when a schema was set.
    Completed {
        text: String,
        structured: Option<Value>,
    },
    /// Failed for its own reasons (item becomes `nil`).
    Failed(String),
    /// Cancelled (by the run, or on its own).
    Cancelled,
    /// Could not start, or infrastructure broke: fatal for the run.
    Fatal(String),
}

/// Where children come from. The engine's implementation is
/// [`SubagentChildren`]; tests substitute a fake.
#[async_trait]
pub trait ChildRunner: Send + Sync {
    /// Synchronous role check, so a bad role is fatal before anything runs.
    fn check_role(&self, role: Option<&str>) -> Result<(), String>;

    /// Run one child to settlement. `cancel` cancels it; `started` must be
    /// called with the child id once the session exists.
    async fn run(
        &self,
        request: ChildRequest,
        cancel: CancellationToken,
        started: Box<dyn FnOnce(SessionId) + Send>,
    ) -> ChildOutcome;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStop {
    Completed,
    Error,
    Cancelled,
}

/// A settled run. `value` is set only when `stop` is `Completed`.
#[derive(Debug, Clone)]
pub struct WorkflowResult {
    pub stop: WorkflowStop,
    pub value: Option<Value>,
    pub error: Option<String>,
    pub agents_started: usize,
}

impl WorkflowResult {
    pub(crate) fn failed(stop: WorkflowStop, error: String, agents_started: usize) -> Self {
        Self {
            stop,
            value: None,
            error: Some(error),
            agents_started,
        }
    }
}

/// Validate `meta` as data (dsh `validateMeta`): all violations at once.
pub fn validate_meta(meta: &Value) -> Result<WorkflowMeta, String> {
    let Some(record) = meta.as_object() else {
        return Err("invalid meta: meta must be an object".into());
    };
    let mut violations = Vec::new();
    for key in record.keys() {
        if !matches!(
            key.as_str(),
            "name" | "description" | "whenToUse" | "phases"
        ) {
            violations.push(format!(
                "meta.{key} is not a recognized field (name/description/whenToUse/phases)"
            ));
        }
    }
    let text = |key: &str| {
        record
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let name = text("name");
    match name {
        None => violations.push("meta.name must be a non-empty string".into()),
        Some(name) if !is_kebab(name) => {
            violations.push("meta.name must be kebab-case (e.g. \"panic-audit\")".into())
        }
        Some(_) => {}
    }
    let description = text("description");
    if description.is_none() {
        violations.push("meta.description must be a non-empty string".into());
    }
    let when_to_use = match record.get("whenToUse") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            violations.push("meta.whenToUse must be a string".into());
            None
        }
    };
    let mut phases = Vec::new();
    match record.get("phases") {
        None => {}
        Some(Value::Array(items)) => {
            for (index, phase) in items.iter().enumerate() {
                let Some(entry) = phase.as_object() else {
                    violations.push(format!("meta.phases[{index}] must be an object"));
                    continue;
                };
                for key in entry.keys() {
                    if !matches!(key.as_str(), "title" | "detail") {
                        violations.push(format!(
                            "meta.phases[{index}].{key} is not a recognized field"
                        ));
                    }
                }
                let title = entry
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                if title.is_none() {
                    violations.push(format!(
                        "meta.phases[{index}].title must be a non-empty string"
                    ));
                }
                let detail = match entry.get("detail") {
                    None => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(_) => {
                        violations.push(format!("meta.phases[{index}].detail must be a string"));
                        None
                    }
                };
                if let Some(title) = title {
                    phases.push(MetaPhase {
                        title: title.to_owned(),
                        detail,
                    });
                }
            }
        }
        Some(_) => violations.push("meta.phases must be an array".into()),
    }
    if !violations.is_empty() {
        return Err(format!("invalid meta: {}", violations.join("; ")));
    }
    Ok(WorkflowMeta {
        name: name.unwrap_or_default().to_owned(),
        description: description.unwrap_or_default().to_owned(),
        when_to_use,
        phases,
    })
}

fn is_kebab(name: &str) -> bool {
    name.split('-').all(|part| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

/// Parse-check a script without running it (the tool rejects a script
/// that does not compile before any run exists).
pub fn check_script(
    meta: &WorkflowMeta,
    script: &str,
    limits: &WorkflowLimits,
) -> Result<(), String> {
    scheduler::compile_check(&meta.name, script, limits)
}

/// Run a workflow to settlement. Never panics on script input: every
/// failure is a non-`Completed` [`WorkflowResult`]. `args` must be a JSON
/// object (or `Null` for none). Cancelling `cancel` cancels in-flight
/// children and ends the run as [`WorkflowStop::Cancelled`].
pub async fn run(
    meta: WorkflowMeta,
    script: String,
    args: Value,
    limits: WorkflowLimits,
    runner: Arc<dyn ChildRunner>,
    observer: Observer,
    cancel: CancellationToken,
) -> WorkflowResult {
    if !matches!(args, Value::Object(_) | Value::Null) {
        return WorkflowResult::failed(WorkflowStop::Error, "args must be a JSON object".into(), 0);
    }
    let handle = tokio::runtime::Handle::current();
    let grace = limits.dispose_grace + Duration::from_secs(2);
    let in_step: Arc<Mutex<Option<Instant>>> = Arc::default();
    let started: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
    let run = scheduler::Run {
        meta,
        script,
        args,
        limits,
        runner,
        observer,
        cancel: cancel.clone(),
        handle,
        in_step: Arc::clone(&in_step),
        agents_started: Arc::clone(&started),
    };
    let started = || started.load(std::sync::atomic::Ordering::Relaxed);
    // A dedicated OS thread, not `spawn_blocking`: a worker stuck in a C call
    // must be detachable without blocking runtime shutdown.
    let (done, worker) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("workflow-{}", run.meta.name))
        .spawn(move || {
            let _ = done.send(run.execute());
        });
    if let Err(e) = spawned {
        return WorkflowResult::failed(
            WorkflowStop::Error,
            format!("workflow worker failed to start: {e}"),
            0,
        );
    }
    let joined = |joined: Result<WorkflowResult, tokio::sync::oneshot::error::RecvError>| {
        joined.unwrap_or_else(|_| {
            WorkflowResult::failed(
                WorkflowStop::Error,
                "workflow worker failed (panicked)".into(),
                0,
            )
        })
    };
    // The instruction hook enforces the budget and cancellation — except
    // while the script is inside one long C call (a pathological string
    // pattern), where hooks cannot fire. Never let that hang the parent:
    // cancel the members (token chain) and detach the worker.
    let stuck = async {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            let past = in_step
                .lock()
                .unwrap()
                .is_some_and(|deadline| Instant::now() > deadline + STUCK_GRACE);
            if past {
                return;
            }
        }
    };
    tokio::pin!(worker);
    tokio::select! {
        result = &mut worker => joined(result),
        () = stuck => {
            cancel.cancel();
            WorkflowResult::failed(
                WorkflowStop::Error,
                "workflow script exceeded its CPU budget inside a single library call (e.g. a pathological string pattern); the script was abandoned".into(),
                started(),
            )
        }
        () = cancel.cancelled() => {
            match tokio::time::timeout(grace, &mut worker).await {
                Ok(result) => joined(result),
                Err(_) => WorkflowResult::failed(
                    WorkflowStop::Cancelled,
                    "workflow run cancelled (script worker did not stop in time and was detached)"
                        .into(),
                    started(),
                ),
            }
        }
    }
}
