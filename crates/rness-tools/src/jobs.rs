//! Background jobs, dsh-style: tools that run long work register it as a
//! job; the model reads, lists, and kills it through kind-independent
//! controls (`job_output`, `job_list`, `job_kill`). Producers (today:
//! `bash` with `run_in_background`) start jobs; these tools only observe
//! and cancel them.
//!
//! Output reads are incremental: `job_output` returns only what arrived
//! since the previous read, and every response ends with a
//! `[status: ...]` marker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rness_engine::tools::Tool;
use serde_json::{json, Value};

const MAX_READ_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    /// Exited on its own; payload is the exit code (None = killed by signal).
    Exited(Option<i32>),
    Killed,
}

impl JobStatus {
    fn marker(&self) -> String {
        match self {
            JobStatus::Running => "[status: running]".into(),
            JobStatus::Exited(Some(0)) => "[status: exited, code 0]".into(),
            JobStatus::Exited(Some(code)) => format!("[status: exited, code {code}]"),
            JobStatus::Exited(None) => "[status: exited by signal]".into(),
            JobStatus::Killed => "[status: killed]".into(),
        }
    }
}

/// One job's shared mutable state; the producer's drain task appends
/// output and settles status, readers consume from `read_from`.
struct JobState {
    kind: &'static str,
    label: String,
    status: JobStatus,
    output: Vec<u8>,
    /// Byte offset of the next unread output.
    read_from: usize,
}

struct Job {
    state: Mutex<JobState>,
    /// Requests cancellation; the drain task settles status when the
    /// process actually dies.
    cancel: tokio_util::sync::CancellationToken,
    /// Notified on every output append and on settlement (for waits).
    changed: Arc<tokio::sync::Notify>,
}

/// Owner of all background jobs for one composition. Cloneable handle.
#[derive(Clone)]
pub struct JobRegistry {
    inner: Arc<Registry>,
}

struct Registry {
    next_id: AtomicU64,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
}

/// Producer-side handle for appending output and settling a started job.
pub struct JobWriter {
    job: Arc<Job>,
}

impl JobWriter {
    pub fn append(&self, bytes: &[u8]) {
        let mut state = self.job.state.lock().expect("job lock");
        state.output.extend_from_slice(bytes);
        drop(state);
        self.job.changed.notify_waiters();
    }

    pub fn settle(&self, status: JobStatus) {
        let mut state = self.job.state.lock().expect("job lock");
        // A kill request wins over the natural exit that follows it.
        state.status = if state.status == JobStatus::Killed { JobStatus::Killed } else { status };
        drop(state);
        self.job.changed.notify_waiters();
    }

    pub fn cancelled(&self) -> tokio_util::sync::CancellationToken {
        self.job.cancel.clone()
    }
}

impl JobRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Registry {
                next_id: AtomicU64::new(1),
                jobs: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Register a new running job; returns its id and the producer handle.
    pub fn start(&self, kind: &'static str, label: String) -> (String, JobWriter) {
        let id = format!("j{}", self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let job = Arc::new(Job {
            state: Mutex::new(JobState {
                kind,
                label,
                status: JobStatus::Running,
                output: Vec::new(),
                read_from: 0,
            }),
            cancel: tokio_util::sync::CancellationToken::new(),
            changed: Arc::new(tokio::sync::Notify::new()),
        });
        self.inner.jobs.lock().expect("jobs lock").insert(id.clone(), job.clone());
        (id, JobWriter { job })
    }

    fn get(&self, id: &str) -> Result<Arc<Job>, String> {
        self.inner
            .jobs
            .lock()
            .expect("jobs lock")
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no job '{id}' — list jobs with job_list"))
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Consume the unread output window (bounded), advancing the read cursor.
fn drain_output(job: &Job) -> (String, JobStatus) {
    let mut state = job.state.lock().expect("job lock");
    let unread = &state.output[state.read_from..];
    // Keep the TAIL of an oversized window: the newest output is what the
    // model needs to see; the cut is reported below.
    let (skipped, window) = if unread.len() > MAX_READ_BYTES {
        (unread.len() - MAX_READ_BYTES, &unread[unread.len() - MAX_READ_BYTES..])
    } else {
        (0, unread)
    };
    let mut text = String::from_utf8_lossy(window).into_owned();
    if skipped > 0 {
        text = format!("… {skipped} bytes skipped …\n{text}");
    }
    state.read_from = state.output.len();
    (text, state.status)
}

// -- job_output --------------------------------------------------------------

pub struct JobOutputTool {
    jobs: JobRegistry,
}

impl JobOutputTool {
    pub fn new(jobs: JobRegistry) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for JobOutputTool {
    fn name(&self) -> &str {
        "job_output"
    }

    fn description(&self) -> &str {
        "Read a background job's output since the previous read. Every \
         response ends with a [status: ...] marker. Non-blocking unless \
         wait is true, which waits for new output or completion."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "The job to read" },
                "wait": { "type": "boolean", "description": "Wait for new output or completion (default false)" },
                "timeout_ms": { "type": "integer", "description": "Max wait in milliseconds (default 30000)" },
            },
            "required": ["job_id"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, String> {
        let id = crate::required_str(&args, "job_id")?;
        let job = self.jobs.get(id)?;
        let wait = args["wait"].as_bool().unwrap_or(false);
        let timeout = std::time::Duration::from_millis(args["timeout_ms"].as_u64().unwrap_or(30_000));

        let (mut text, mut status) = drain_output(&job);
        if wait && text.is_empty() && status == JobStatus::Running {
            // Subscribe BEFORE re-checking to avoid a lost wakeup, then wait
            // for change or timeout; a still-running job stays alive.
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let notified = job.changed.notified();
                (text, status) = drain_output(&job);
                if !text.is_empty() || status != JobStatus::Running {
                    break;
                }
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    break;
                }
            }
        }
        Ok(if text.is_empty() {
            format!("(no new output)\n{}", status.marker())
        } else {
            format!("{text}\n{}", status.marker())
        })
    }
}

// -- job_list ----------------------------------------------------------------

pub struct JobListTool {
    jobs: JobRegistry,
}

impl JobListTool {
    pub fn new(jobs: JobRegistry) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for JobListTool {
    fn name(&self) -> &str {
        "job_list"
    }

    fn description(&self) -> &str {
        "List background jobs: <id> [<kind>] <status> — <label>, one per line."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        let jobs = self.jobs.inner.jobs.lock().expect("jobs lock");
        if jobs.is_empty() {
            return Ok("No background jobs".into());
        }
        let mut lines: Vec<(u64, String)> = jobs
            .iter()
            .map(|(id, job)| {
                let state = job.state.lock().expect("job lock");
                let n: u64 = id[1..].parse().unwrap_or(0);
                (n, format!("{id} [{}] {} — {}", state.kind, state.status.marker(), state.label))
            })
            .collect();
        lines.sort();
        Ok(lines.into_iter().map(|(_, l)| l).collect::<Vec<_>>().join("\n"))
    }
}

// -- job_kill ----------------------------------------------------------------

pub struct JobKillTool {
    jobs: JobRegistry,
}

impl JobKillTool {
    pub fn new(jobs: JobRegistry) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for JobKillTool {
    fn name(&self) -> &str {
        "job_kill"
    }

    fn description(&self) -> &str {
        "Request cancellation of a running background job. The job settles \
         as killed once its process actually stops."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "The job to kill" },
            },
            "required": ["job_id"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, String> {
        let id = crate::required_str(&args, "job_id")?;
        let job = self.jobs.get(id)?;
        let already = {
            let mut state = job.state.lock().expect("job lock");
            match state.status {
                JobStatus::Running => {
                    state.status = JobStatus::Killed;
                    false
                }
                _ => true,
            }
        };
        if already {
            let state = job.state.lock().expect("job lock");
            return Ok(format!("job {id} already finished {}", state.status.marker()));
        }
        job.cancel.cancel();
        job.changed.notify_waiters();
        Ok(format!("cancellation requested for job {id}"))
    }
}
