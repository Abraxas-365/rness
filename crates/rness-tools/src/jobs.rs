//! Background jobs, dsh-style: producers (today `bash` with
//! `run_in_background`) start jobs, and the owning session is automatically
//! notified when one settles. The model must not poll jobs: after that notice,
//! it reads the result with `job_output`; `job_list` is only for explicit
//! inspection, and `job_kill` cancels a job.
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

impl JobState {
    fn visible_to(&self, session: Option<&str>) -> bool {
        self.owner.is_none() || self.owner.as_deref() == session
    }
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

    fn get_for_session(&self, id: &str, session: Option<&str>) -> Result<Arc<Job>, String> {
        let job = self.get(id)?;
        if !job.state.lock().expect("job lock").visible_to(session) {
            return Err(format!("job '{id}' belongs to another session"));
        }
        Ok(job)
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

#[cfg(test)]
mod isolation_tests {
    use super::*;

    #[tokio::test]
    async fn foreign_sessions_cannot_read_wait_or_kill_owned_jobs() {
        let registry = JobRegistry::new();
        let owner = "owner".to_string();
        let foreign = "child-or-unrelated".to_string();
        let (id, writer) = registry.start_owned("subagent", "private".into(), Some(&owner));
        writer.append(b"secret");
        let output = JobOutputTool::new(registry.clone());
        let kill = JobKillTool::new(registry.clone());
        let cancel = tokio_util::sync::CancellationToken::new();
        let call = "call".to_string();
        for wait in [false, true] {
            let args = json!({"job_id":id,"wait":wait});
            assert!(output.execute(args.clone()).await.is_err());
            assert!(output.execute_in(&foreign, args.clone()).await.unwrap_err().contains("belongs to another session"));
            assert!(output.execute_call(&foreign, &call, args.clone(), &cancel).await.is_err());
            assert!(output.execute_presented(&foreign, &call, args, &cancel).await.is_err());
        }
        let args = json!({"job_id":id});
        assert!(kill.execute(args.clone()).await.is_err());
        assert!(kill.execute_in(&foreign, args.clone()).await.is_err());
        assert!(kill.execute_call(&foreign, &call, args.clone(), &cancel).await.is_err());
        assert!(kill.execute_presented(&foreign, &call, args.clone(), &cancel).await.is_err());
        assert!(!writer.cancelled().is_cancelled());
        assert_eq!(writer.job.state.lock().unwrap().read_from, 0);
        assert!(output.execute_in(&owner, args.clone()).await.unwrap().contains("secret"));
        kill.execute_presented(&owner, &call, args, &cancel).await.unwrap();
        assert!(writer.cancelled().is_cancelled());
    }

    #[tokio::test]
    async fn lists_filter_text_metadata_and_totals_and_share_unowned_jobs() {
        let registry = JobRegistry::new();
        let owner = "owner".to_string();
        let foreign = "foreign".to_string();
        let (private, _) = registry.start_owned("bash", "private".into(), Some(&owner));
        let list = JobListTool::new(registry.clone());
        let (text, metadata) = list.list_presented(Some(&foreign)).unwrap();
        assert_eq!(text, "No background jobs");
        assert_eq!(metadata["total"], 0);
        assert_eq!(metadata["jobs"], json!([]));
        assert_eq!(metadata["truncated"], false);
        let (shared, writer) = registry.start("bash", "shared".into());
        writer.append(b"public");
        for session in [None, Some(foreign.as_str()), Some(owner.as_str())] {
            let (text, metadata) = list.list_presented(session).unwrap();
            let owns = session == Some(owner.as_str());
            assert_eq!(text.contains("private"), owns);
            assert!(text.contains("shared"));
            assert_eq!(metadata["total"], if owns { 2 } else { 1 });
            assert_eq!(metadata["jobs"].as_array().unwrap().iter().any(|j| j["job_id"] == private), owns);
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let (_, _, _, metadata) = list.execute_presented(&foreign, &"call".into(), json!({}), &cancel).await.unwrap();
        assert_eq!(metadata.unwrap()["total"], 1);
        assert!(!list.execute_in(&foreign, json!({})).await.unwrap().contains("private"));
        let output = JobOutputTool::new(registry.clone());
        assert!(output.execute_in(&foreign, json!({"job_id":shared})).await.unwrap().contains("public"));
        JobKillTool::new(registry).execute(json!({"job_id":shared})).await.unwrap();
        assert!(writer.cancelled().is_cancelled());
    }

    #[test]
    fn recovered_jobs_preserve_session_isolation() {
        let directory = tempfile::tempdir().unwrap();
        let registry = JobRegistry::new();
        registry.enable_persistence(directory.path()).unwrap();
        let (id, writer) = registry.start_owned("bash", "private".into(), Some(&"owner".into()));
        writer.append(b"secret");
        drop(writer);
        drop(registry);
        let recovered = JobRegistry::new();
        recovered.enable_persistence(directory.path()).unwrap();
        assert!(recovered.get_for_session(&id, Some("foreign")).is_err());
        assert!(recovered.get_for_session(&id, None).is_err());
        assert!(recovered.get_for_session(&id, Some("owner")).is_ok());
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
        "Read a background job's output since the previous read. Do not use this \
         tool to poll or wait for a running job: completion automatically notifies \
         the owning session. After receiving that notification, call this once to \
         retrieve the result before reporting completion. Every response ends with \
         a [status: ...] marker."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "The job to read" },
                "wait": { "type": "boolean", "description": "Reserved for explicit synchronous use; do not use it to wait for job completion" },
                "timeout_ms": { "type": "integer", "description": "Max explicit wait in milliseconds (default 30000)" },
            },
            "required": ["job_id"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, String> {
        self.read_presented(None, args).await.map(|(output, _)| output)
    }

    async fn execute_in(&self, session: &String, args: Value) -> Result<String, String> {
        self.read_presented(Some(session), args).await.map(|(output, _)| output)
    }

    async fn execute_presented(&self, session: &String, _call: &String, args: Value, _cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let (output, metadata) = self.read_presented(Some(session), args).await?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text {text:output}], None, false, Some(metadata)))
    }
}

impl JobOutputTool {
    async fn read_presented(&self, session: Option<&str>, args: Value) -> Result<(String, Value), String> {
        let id = crate::required_str(&args, "job_id")?;
        let job = self.jobs.get_for_session(id, session)?;
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
        "List background jobs for explicit inspection. Do not use this tool to \
         poll for completion: the owning session is notified automatically when a \
         job settles."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        self.list_presented(None).map(|(output, _)| output)
    }

    async fn execute_in(&self, session: &String, _args: Value) -> Result<String, String> {
        self.list_presented(Some(session)).map(|(output, _)| output)
    }

    async fn execute_presented(&self, session: &String, _call: &String, _args: Value, _cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let (output, metadata) = self.list_presented(Some(session))?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text {text:output}], None, false, Some(metadata)))
    }
}

impl JobListTool {
    fn list_presented(&self, session: Option<&str>) -> Result<(String, Value), String> {
        let jobs = self.jobs.inner.jobs.lock().expect("jobs lock");
        let mut records = Vec::new();
        let mut metadata_bytes = 0;
        let mut lines: Vec<(u64, String)> = jobs
            .iter()
            .filter_map(|(id, job)| {
                let state = job.state.lock().expect("job lock");
                if !state.visible_to(session) { return None; }
                let n: u64 = id[1..].parse().unwrap_or(0);
                let record = json!({"job_id":id,"kind":state.kind,"status":state.status.marker(),"label":state.label});
                metadata_bytes += serde_json::to_vec(&record).unwrap().len();
                if metadata_bytes <= 48 * 1024 { records.push(record); }
                Some((n, format!("{id} [{}] {} — {}", state.kind, state.status.marker(), state.label)))
            })
            .collect();
        lines.sort();
        let metadata = json!({"version":1,"kind":"job_list","total":lines.len(),"truncated":records.len()<lines.len(),"jobs":records});
        let output = if lines.is_empty() { "No background jobs".into() } else {
            lines.into_iter().map(|(_, l)| l).collect::<Vec<_>>().join("\n")
        };
        Ok((output, metadata))
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
        self.kill_presented(None, args).map(|(output, _)| output)
    }

    async fn execute_in(&self, session: &String, args: Value) -> Result<String, String> {
        self.kill_presented(Some(session), args).map(|(output, _)| output)
    }

    async fn execute_presented(&self, session: &String, _call: &String, args: Value, _cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let (output, metadata) = self.kill_presented(Some(session), args)?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text {text:output}], None, false, Some(metadata)))
    }
}

impl JobKillTool {
    fn kill_presented(&self, session: Option<&str>, args: Value) -> Result<(String, Value), String> {
        let id = crate::required_str(&args, "job_id")?;
        let job = self.jobs.get_for_session(id, session)?;
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
