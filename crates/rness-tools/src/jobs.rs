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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum JobStatus {
    Running,
    /// Exited on its own; payload is the exit code (None = killed by signal).
    Exited(Option<i32>),
    Killed,
    Interrupted,
}

impl JobStatus {
    fn marker(&self) -> String {
        match self {
            JobStatus::Running => "[status: running]".into(),
            JobStatus::Exited(Some(0)) => "[status: exited, code 0]".into(),
            JobStatus::Exited(Some(code)) => format!("[status: exited, code {code}]"),
            JobStatus::Exited(None) => "[status: exited by signal]".into(),
            JobStatus::Killed => "[status: killed]".into(),
            JobStatus::Interrupted => "[status: interrupted]".into(),
        }
    }
}

/// One job's shared mutable state; the producer's drain task appends
/// output and settles status, readers consume from `read_from`.
#[derive(serde::Serialize, serde::Deserialize)]
struct JobState {
    kind: String,
    label: String,
    status: JobStatus,
    #[serde(skip)]
    output: Vec<u8>,
    /// Byte offset of the next unread output.
    read_from: usize,
    settled: bool,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    delivered: bool,
}

struct Job {
    path: Option<std::path::PathBuf>,
    state: Mutex<JobState>,
    /// Requests cancellation; the drain task settles status when the
    /// process actually dies.
    cancel: tokio_util::sync::CancellationToken,
    /// Notified on every output append and on settlement (for waits).
    changed: Arc<tokio::sync::Notify>,
    completion: Option<(String, String, std::sync::Weak<rness_engine::service::SessionService>)>,
}

/// Owner of all background jobs for one composition. Cloneable handle.
#[derive(Clone)]
pub struct JobRegistry {
    inner: Arc<Registry>,
}

struct Registry {
    directory: Mutex<Option<std::path::PathBuf>>,
    locks: Mutex<Vec<std::fs::File>>,
    next_id: AtomicU64,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    sessions: Mutex<std::sync::Weak<rness_engine::service::SessionService>>,
}

/// Producer-side handle for appending output and settling a started job.
pub struct JobWriter {
    job: Arc<Job>,
}

impl Job {
    fn persist(&self, state: &JobState) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()); };
        use std::io::Write;
        let temporary = path.with_extension("tmp");
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&temporary)?;
        file.write_all(&serde_json::to_vec(state)?)?;
        file.sync_all()?;
        std::fs::rename(temporary,path)?;
        std::fs::File::open(path.parent().unwrap())?.sync_all()
    }
    fn checkpoint(&self, state: &JobState) {
        if let Err(error) = self.persist(state) { self.cancel.cancel(); tracing::error!(%error,"job persistence failed; cancelling producer"); }
    }
}

impl JobWriter {
    pub fn append(&self, bytes: &[u8]) {
        let mut state = self.job.state.lock().expect("job lock");
        if let Some(path) = &self.job.path {
            use std::io::Write;
            let append = (|| -> std::io::Result<()> {
                let mut file = std::fs::OpenOptions::new().append(true).open(path.with_extension("output"))?;
                file.write_all(bytes)?;
                file.sync_data()
            })();
            if let Err(error) = append { self.job.cancel.cancel(); tracing::error!(%error,"job output append failed"); return; }
        }
        state.output.extend_from_slice(bytes);
        drop(state);
        self.job.changed.notify_waiters();
    }

    pub fn settle(&self, status: JobStatus) {
        let mut state = self.job.state.lock().expect("job lock");
        if state.settled { return; }
        state.settled = true;
        // A kill request wins over the natural exit that follows it.
        state.status = if state.status == JobStatus::Killed { JobStatus::Killed } else { status };
        self.job.checkpoint(&state);
        let notice = format!("Background {} job finished {}. Read its output with job_output.", state.kind, state.status.marker());
        drop(state);
        self.job.changed.notify_waiters();
        if self.job.path.is_some() { return; }
        if let Some((id, owner, sessions)) = &self.job.completion {
            if let Some(sessions) = sessions.upgrade() {
                let text = format!("Job {id}: {notice}");
                match sessions.notify_job(owner, text.clone()) {
                    Err(rness_engine::service::ServiceError::Busy) => {
                        let owner = owner.clone();
                        let id = id.clone();
                        tokio::spawn(async move {
                            if let Err(error) = sessions.notify_job_wait(&owner, text).await {
                                tracing::warn!(job = %id, session = %owner, %error, "job completion delivery failed");
                            }
                        });
                    }
                    Err(error) => tracing::warn!(job = %id, session = %owner, %error, "job completion delivery failed"),
                    Ok(_) => {}
                }
            }
        }
    }

    pub fn cancelled(&self) -> tokio_util::sync::CancellationToken {
        self.job.cancel.clone()
    }
}

impl JobRegistry {
    pub fn enable_persistence(&self, root: &std::path::Path) -> Result<(), String> {
        use fs2::FileExt;
        std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
        let mut directory = self.inner.directory.lock().unwrap();
        if directory.is_some() || !self.inner.jobs.lock().unwrap().is_empty() { return Err("enable job persistence before starting jobs".into()); }
        for entry in std::fs::read_dir(root).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_dir() { continue; }
            let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(entry.path().join("owner.lock")).map_err(|e| e.to_string())?;
            match lock.try_lock_exclusive() {
                Ok(()) => {},
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error.to_string()),
            }
            for file in std::fs::read_dir(entry.path()).map_err(|e| e.to_string())? {
                let path = file.map_err(|e| e.to_string())?.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
                let mut state: JobState = serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {e}",path.display()))?;
                state.output = std::fs::read(path.with_extension("output")).map_err(|e| format!("job output: {e}"))?;
                if state.read_from > state.output.len() { return Err("invalid durable job output cursor".into()); }
                if !state.settled { state.status = JobStatus::Interrupted; state.settled = true; }
                let id = path.file_stem().unwrap().to_str().ok_or("invalid job ID")?.to_owned();
                let job = Arc::new(Job {path:Some(path),state:Mutex::new(state),cancel:Default::default(),changed:Arc::new(tokio::sync::Notify::new()),completion:None});
                job.persist(&job.state.lock().unwrap()).map_err(|e| e.to_string())?;
                self.inner.jobs.lock().unwrap().insert(id,job);
            }
            self.inner.locks.lock().unwrap().push(lock);
        }
        let path = root.join(ulid::Ulid::new().to_string());
        std::fs::create_dir(&path).map_err(|e| e.to_string())?;
        let lock = std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(path.join("owner.lock")).map_err(|e| e.to_string())?;
        lock.try_lock_exclusive().map_err(|e| e.to_string())?;
        self.inner.locks.lock().unwrap().push(lock);
        *directory = Some(path);
        Ok(())
    }

    pub(crate) fn stream_output(&self, session: &str, call: &str, output: String) {
        if let Some(sessions) = self.inner.sessions.lock().unwrap().upgrade() {
            sessions.bus().emit::<rness_engine::subagent::ToolStreamEv>(&(session.into(), call.into(), output));
        }
    }

    pub fn new() -> Self {
        Self {
            inner: Arc::new(Registry {
                directory: Mutex::new(None),
                locks: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                jobs: Mutex::new(HashMap::new()),
                sessions: Mutex::new(std::sync::Weak::new()),
            }),
        }
    }

    pub fn attach_sessions(&self, sessions: &Arc<rness_engine::service::SessionService>) {
        *self.inner.sessions.lock().expect("sessions lock") = Arc::downgrade(sessions);
        if self.inner.directory.lock().unwrap().is_some() {
            let registry = Arc::downgrade(&self.inner);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                loop {
                    interval.tick().await;
                    let Some(registry) = registry.upgrade() else { break; };
                    let sessions = registry.sessions.lock().unwrap().upgrade();
                    let Some(sessions) = sessions else { break; };
                    let pending: Vec<_> = registry.jobs.lock().unwrap().iter().filter_map(|(id,job)| {
                        let state = job.state.lock().unwrap();
                        if state.settled && !state.delivered { state.owner.clone().map(|owner| (id.clone(),owner,job.clone(),format!("Job {id}: Background {} job finished {}. Read its output with job_output.",state.kind,state.status.marker()))) } else { None }
                    }).collect();
                    drop(registry);
                    for (id,owner,job,text) in pending {
                        match sessions.notify_job_once(&owner,&id,text).await {
                            Ok(true) => { let mut state = job.state.lock().unwrap(); state.delivered = true; job.checkpoint(&state); },
                            Ok(false) => {},
                            Err(error) => tracing::warn!(%id,%error,"durable job completion remains pending"),
                        }
                    }
                }
            });
        }
    }

    /// Register a new running job; returns its id and the producer handle.
    pub fn start(&self, kind: &'static str, label: String) -> (String, JobWriter) {
        self.start_owned(kind, label, None)
    }

    pub fn start_owned(&self, kind: &'static str, label: String, owner: Option<&String>) -> (String, JobWriter) {
        let id = if self.inner.directory.lock().unwrap().is_some() { format!("j{}",ulid::Ulid::new()) } else { format!("j{}", self.inner.next_id.fetch_add(1, Ordering::Relaxed)) };
        let job = Arc::new(Job {
            path: self.inner.directory.lock().unwrap().as_ref().map(|dir| dir.join(format!("{id}.json"))),
            state: Mutex::new(JobState {
                kind: kind.into(),
                owner: owner.cloned(),
                delivered: false,
                label,
                status: JobStatus::Running,
                output: Vec::new(),
                read_from: 0,
                settled: false,
            }),
            completion: owner.map(|owner| (id.clone(), owner.clone(), self.inner.sessions.lock().expect("sessions lock").clone())),
            cancel: tokio_util::sync::CancellationToken::new(),
            changed: Arc::new(tokio::sync::Notify::new()),
        });
        if let Some(path) = &job.path {
            let created = std::fs::OpenOptions::new().create_new(true).write(true).open(path.with_extension("output")).and_then(|file| file.sync_all());
            if let Err(error) = created { job.cancel.cancel(); tracing::error!(%error,"job output creation failed"); }
        }
        job.checkpoint(&job.state.lock().unwrap());
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

#[cfg(test)]
mod durability_tests {
    use super::*;
    #[test]
    fn output_append_does_not_rewrite_metadata_and_recovers_before_settlement() {
        let directory = tempfile::tempdir().unwrap();
        let registry = JobRegistry::new(); registry.enable_persistence(directory.path()).unwrap();
        let (id,writer) = registry.start("bash","test".into());
        let path = writer.job.path.clone().unwrap();
        let before = std::fs::read(&path).unwrap();
        writer.append(b"first"); writer.append(b"second");
        assert_eq!(std::fs::read(&path).unwrap(),before);
        assert_eq!(std::fs::read(path.with_extension("output")).unwrap(),b"firstsecond");
        // Crash after output fsync but before settlement/metadata rename.
        std::fs::write(path.with_extension("tmp"),b"{partial metadata").unwrap();
        drop(writer); drop(registry);
        let recovered = JobRegistry::new(); recovered.enable_persistence(directory.path()).unwrap();
        let (output,status,_) = drain_output(&recovered.get(&id).unwrap());
        assert_eq!(output,"firstsecond"); assert_eq!(status,JobStatus::Interrupted);
    }
    #[test]
    fn failed_append_cancels_producer_without_publishing_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let registry = JobRegistry::new(); registry.enable_persistence(directory.path()).unwrap();
        let (_,writer) = registry.start("bash","test".into());
        let output = writer.job.path.as_ref().unwrap().with_extension("output");
        std::fs::remove_file(&output).unwrap(); std::fs::create_dir(&output).unwrap();
        writer.append(b"not persisted");
        assert!(writer.cancelled().is_cancelled()); assert!(writer.job.state.lock().unwrap().output.is_empty());
    }
    #[test]
    fn interrupted_recovery_preserves_output_cursor_and_live_owner_lock() {
        let directory = tempfile::tempdir().unwrap();
        let first = JobRegistry::new(); first.enable_persistence(directory.path()).unwrap();
        let (id,writer) = first.start_owned("bash","command".into(),Some(&"session".into()));
        writer.append(b"recorded");
        let second = JobRegistry::new(); second.enable_persistence(directory.path()).unwrap();
        assert!(second.get(&id).is_err());
        drop(second); drop(writer); drop(first);
        let recovered = JobRegistry::new(); recovered.enable_persistence(directory.path()).unwrap();
        let job = recovered.get(&id).unwrap();
        assert_eq!(job.state.lock().unwrap().owner.as_deref(),Some("session"));
        let (output,status,_) = drain_output(&job);
        assert_eq!(output,"recorded"); assert_eq!(status,JobStatus::Interrupted);
        drop(job); drop(recovered);
        let again = JobRegistry::new(); again.enable_persistence(directory.path()).unwrap();
        assert_eq!(drain_output(&again.get(&id).unwrap()).0,"");
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Consume the unread output window (bounded), advancing the read cursor.
fn drain_output(job: &Job) -> (String, JobStatus, Value) {
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
    let presentation = json!({
        "version":1,"kind":"job_output","status":state.status.marker(),
        "start_byte":state.read_from + skipped,"end_byte":state.output.len(),
        "skipped_bytes":skipped,"truncated":skipped > 0,
    });
    state.read_from = state.output.len();
    job.checkpoint(&state);
    (text, state.status, presentation)
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
        self.read_presented(args).await.map(|(output, _)| output)
    }

    async fn execute_presented(&self, _session: &String, _call: &String, args: Value, _cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let (output, metadata) = self.read_presented(args).await?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text {text:output}], None, false, Some(metadata)))
    }
}

impl JobOutputTool {
    async fn read_presented(&self, args: Value) -> Result<(String, Value), String> {
        let id = crate::required_str(&args, "job_id")?;
        let job = self.jobs.get(id)?;
        let wait = args["wait"].as_bool().unwrap_or(false);
        let timeout = std::time::Duration::from_millis(args["timeout_ms"].as_u64().unwrap_or(30_000));

        let (mut text, mut status, mut metadata) = drain_output(&job);
        if wait && text.is_empty() && status == JobStatus::Running {
            // Subscribe BEFORE re-checking to avoid a lost wakeup, then wait
            // for change or timeout; a still-running job stays alive.
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let notified = job.changed.notified();
                (text, status, metadata) = drain_output(&job);
                if !text.is_empty() || status != JobStatus::Running {
                    break;
                }
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    break;
                }
            }
        }
        metadata["job_id"] = json!(id);
        Ok((if text.is_empty() {
            format!("(no new output)\n{}", status.marker())
        } else {
            format!("{text}\n{}", status.marker())
        }, metadata))
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
        self.list_presented().map(|(output, _)| output)
    }

    async fn execute_presented(&self, _session: &String, _call: &String, _args: Value, _cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let (output, metadata) = self.list_presented()?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text {text:output}], None, false, Some(metadata)))
    }
}

impl JobListTool {
    fn list_presented(&self) -> Result<(String, Value), String> {
        let jobs = self.jobs.inner.jobs.lock().expect("jobs lock");
        let mut records = Vec::new();
        let mut metadata_bytes = 0;
        if jobs.is_empty() {
            return Ok(("No background jobs".into(), json!({"version":1,"kind":"job_list","jobs":[],"truncated":false})));
        }
        let mut lines: Vec<(u64, String)> = jobs
            .iter()
            .map(|(id, job)| {
                let state = job.state.lock().expect("job lock");
                let n: u64 = id[1..].parse().unwrap_or(0);
                let record = json!({"job_id":id,"kind":state.kind,"status":state.status.marker(),"label":state.label});
                metadata_bytes += serde_json::to_vec(&record).unwrap().len();
                if metadata_bytes <= 48 * 1024 { records.push(record); }
                (n, format!("{id} [{}] {} — {}", state.kind, state.status.marker(), state.label))
            })
            .collect();
        lines.sort();
        let metadata = json!({"version":1,"kind":"job_list","total":jobs.len(),"truncated":records.len()<jobs.len(),"jobs":records});
        Ok((lines.into_iter().map(|(_, l)| l).collect::<Vec<_>>().join("\n"), metadata))
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
        self.kill_presented(args).map(|(output, _)| output)
    }

    async fn execute_presented(&self, _session: &String, _call: &String, args: Value, _cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let (output, metadata) = self.kill_presented(args)?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text {text:output}], None, false, Some(metadata)))
    }
}

impl JobKillTool {
    fn kill_presented(&self, args: Value) -> Result<(String, Value), String> {
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
            return Ok((format!("job {id} already finished {}", state.status.marker()), json!({"version":1,"kind":"job_kill","job_id":id,"cancellation_requested":false,"status":state.status.marker()})));
        }
        job.cancel.cancel();
        job.changed.notify_waiters();
        Ok((format!("cancellation requested for job {id}"), json!({"version":1,"kind":"job_kill","job_id":id,"cancellation_requested":true})))
    }
}
