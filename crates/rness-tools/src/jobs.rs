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

mod retention;
pub use retention::Retention;

const MAX_READ_BYTES: usize = 64 * 1024;
const MAX_INSPECT_BYTES: usize = 8 * 1024;

pub(crate) fn retain_tail(output: &mut Vec<u8>, bytes: &[u8], limit: usize) {
    if bytes.len() >= limit {
        output.clear();
        output.extend_from_slice(&bytes[bytes.len() - limit..]);
    } else {
        let remove = (output.len() + bytes.len()).saturating_sub(limit);
        output.drain(..remove);
        output.extend_from_slice(bytes);
    }
}

/// Non-consuming, session-visible job metadata for user controls.
#[derive(Debug, serde::Serialize)]
pub struct JobSnapshot {
    pub job_id: String,
    pub kind: String,
    pub label: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// True until the producer settles, including cancellation pending.
    pub running: bool,
    pub cancellation_requested: bool,
    pub output_error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct JobInspection {
    #[serde(flatten)]
    pub job: JobSnapshot,
    pub output: String,
    pub output_bytes: usize,
}

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
    #[serde(skip)]
    output_bytes: usize,
    #[serde(skip)]
    charged_bytes: u64,
    /// Byte offset of the next unread output.
    read_from: usize,
    settled: bool,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    delivered: bool,
    #[serde(default)]
    settled_at_ms: Option<u64>,
    #[serde(default)]
    output_error: Option<String>,
}

impl JobState {
    fn marker(&self) -> String {
        match &self.output_error {
            Some(error) => format!("{} [output error: {error}]", self.status.marker()),
            None => self.status.marker(),
        }
    }
    fn visible_to(&self, session: Option<&str>) -> bool {
        self.owner.is_none() || self.owner.as_deref() == session
    }
    /// `bash-output` entries are foreground-command output captures, not
    /// background jobs: they settle instantly and duplicate output already
    /// returned inline. They stay retrievable by id via `job_output`, but
    /// are noise in job-listing surfaces meant for actual background work.
    fn listable(&self, session: Option<&str>) -> bool {
        self.visible_to(session) && self.kind != "bash-output"
    }
}

struct Job {
    budget: Arc<Mutex<retention::Budget>>,
    path: Option<std::path::PathBuf>,
    state: Mutex<JobState>,
    /// Ephemeral compositions still spool full output to disk without JSON metadata.
    spool: Mutex<Option<std::fs::File>>,
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
    budget: Arc<Mutex<retention::Budget>>,
    directory: Mutex<Option<std::path::PathBuf>>,
    locks: Mutex<Vec<std::fs::File>>,
    next_id: AtomicU64,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    sessions: Mutex<std::sync::Weak<rness_engine::service::SessionService>>,
}

/// Producer-side handle for appending output and settling a started job.
#[derive(Clone)]
pub struct JobWriter {
    job: Arc<Job>,
}

impl Job {
    fn snapshot(&self, id: &str, state: &JobState) -> JobSnapshot {
        let cancellation_requested = self.cancel.is_cancelled();
        let status = if !state.settled && cancellation_requested {
            "cancelling"
        } else {
            match state.status {
                JobStatus::Running => "running",
                JobStatus::Exited(_) => "exited",
                JobStatus::Killed => "killed",
                JobStatus::Interrupted => "interrupted",
            }
        };
        JobSnapshot {
            job_id: id.into(), kind: state.kind.clone(), label: state.label.clone(),
            status: status.into(), running: !state.settled, cancellation_requested,
            output_error: state.output_error.clone(),
            exit_code: match state.status { JobStatus::Exited(code) => code, _ => None },
        }
    }

    /// Shared by the model tool and user controls. Settlement stays producer-owned.
    fn request_stop(&self) -> bool {
        let mut state = self.state.lock().expect("job lock");
        if state.settled || state.status != JobStatus::Running || self.cancel.is_cancelled() {
            return false;
        }
        state.status = JobStatus::Killed;
        self.cancel.cancel();
        drop(state);
        self.changed.notify_waiters();
        true
    }

    fn persist(&self, state: &JobState) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()); };
        use std::io::Write;
        let temporary = path.with_extension("tmp");
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&temporary)?;
        file.write_all(&serde_json::to_vec(state)?)?;
        file.sync_all()?;
        std::fs::rename(temporary,path)?;
        sync_directory(path.parent().unwrap())
    }
    fn reconcile_output(&self, state: &mut JobState) {
        use std::io::{Read, Seek, SeekFrom};
        let recover = |file: &mut std::fs::File| -> std::io::Result<(usize, Vec<u8>)> {
            let len = usize::try_from(file.metadata()?.len()).map_err(std::io::Error::other)?;
            file.seek(SeekFrom::Start(len.saturating_sub(MAX_READ_BYTES) as u64))?;
            let mut tail = Vec::new();
            file.take(MAX_READ_BYTES as u64).read_to_end(&mut tail)?;
            Ok((len, tail))
        };
        let result = if let Some(path) = &self.path {
            std::fs::File::open(path.with_extension("output")).and_then(|mut file| recover(&mut file))
        } else {
            self.spool.lock().unwrap().as_mut().ok_or_else(|| std::io::Error::other("output spool unavailable")).and_then(recover)
        };
        if let Ok((len, tail)) = result {
            state.output_bytes = len;
            state.output = tail;
            state.read_from = state.read_from.min(len);
        }
    }

    fn checkpoint(&self, state: &JobState) {
        if let Err(error) = self.persist(state) { self.cancel.cancel(); tracing::error!(%error,"job persistence failed; cancelling producer"); }
    }
}

fn sync_directory(path: &std::path::Path) -> std::io::Result<()> {
    // Windows does not expose portable directory fsync through std::fs.
    #[cfg(unix)]
    { std::fs::File::open(path)?.sync_all() }
    #[cfg(not(unix))]
    { let _ = path; Ok(()) }
}

impl JobWriter {
    pub fn append(&self, bytes: &[u8]) {
        let mut state = self.job.state.lock().expect("job lock");
        if state.settled || state.output_error.is_some() { return; }
        let mut budget = self.job.budget.lock().unwrap();
        let count = bytes.len() as u64;
        let reason = if budget.policy.max_job_bytes != 0 && (state.output_bytes as u64).saturating_add(count) > budget.policy.max_job_bytes {
            Some("per-job output quota exceeded")
        } else if budget.policy.max_total_bytes != 0 && budget.used.saturating_add(count) > budget.policy.max_total_bytes {
            Some("total output quota exceeded")
        } else { None };
        if let Some(reason) = reason {
            state.output_error = Some(format!("{reason}; producer cancelled; output is incomplete"));
            self.job.checkpoint(&state);
            self.job.cancel.cancel();
            self.job.changed.notify_waiters();
            return;
        }
        // Reserve before writing, including partial-write failures. This is
        // conservative until recovery measures the actual spool length.
        budget.used = budget.used.saturating_add(count);
        state.charged_bytes = state.charged_bytes.saturating_add(count);
        drop(budget);
        if let Some(path) = &self.job.path {
            use std::io::Write;
            let append = (|| -> std::io::Result<()> {
                let mut file = std::fs::OpenOptions::new().append(true).open(path.with_extension("output"))?;
                file.write_all(bytes)?;
                file.sync_data()
            })();
            if let Err(error) = append {
                state.output_error = Some(format!("output persistence failed: {error}; output is incomplete"));
                self.job.reconcile_output(&mut state);
                self.job.checkpoint(&state);
                self.job.cancel.cancel();
                self.job.changed.notify_waiters();
                return;
            }
        } else {
            use std::io::{Seek, SeekFrom, Write};
            let append = (|| -> std::io::Result<()> {
                let mut spool = self.job.spool.lock().unwrap();
                let file = spool.as_mut().ok_or_else(|| std::io::Error::other("output spool unavailable"))?;
                file.seek(SeekFrom::End(0))?;
                file.write_all(bytes)
            })();
            if let Err(error) = append {
                state.output_error = Some(format!("output spool failed: {error}; output is incomplete"));
                self.job.reconcile_output(&mut state);
                self.job.cancel.cancel();
                self.job.changed.notify_waiters();
                return;
            }
        }
        state.output_bytes += bytes.len();
        retain_tail(&mut state.output, bytes, MAX_READ_BYTES);
        drop(state);
        self.job.changed.notify_waiters();
    }

    pub fn settle(&self, status: JobStatus) {
        let mut state = self.job.state.lock().expect("job lock");
        if state.settled { return; }
        state.settled = true;
        state.settled_at_ms = Some(retention::now_ms());
        // A kill request wins over the natural exit that follows it.
        state.status = if state.status == JobStatus::Killed { JobStatus::Killed } else { status };
        self.job.checkpoint(&state);
        let notice = format!("Background {} job finished {}. Read its output with job_output.", state.kind, state.marker());
        let notify = !state.delivered;
        drop(state);
        self.job.changed.notify_waiters();
        if self.job.path.is_some() || !notify { return; }
        if let Some((id, owner, sessions)) = &self.job.completion {
            if let Some(sessions) = sessions.upgrade() {
                let text = format!("Job {id}: {notice}");
                match sessions.notify_job(owner, text.clone()) {
                    Err(rness_engine::service::ServiceError::Busy) => {
                        let owner = owner.clone();
                        let id = id.clone();
                        let job = self.job.clone();
                        tokio::spawn(async move {
                            match sessions.notify_job_wait(&owner, text).await {
                                Ok(_) => job.state.lock().unwrap().delivered = true,
                                Err(error) => tracing::warn!(job = %id, session = %owner, %error, "job completion delivery failed"),
                            }
                        });
                    }
                    Err(error) => tracing::warn!(job = %id, session = %owner, %error, "job completion delivery failed"),
                    Ok(_) => self.job.state.lock().unwrap().delivered = true,
                }
            }
        }
    }

    pub fn output_error(&self) -> Option<String> {
        self.job.state.lock().unwrap().output_error.clone()
    }

    pub fn cancelled(&self) -> tokio_util::sync::CancellationToken {
        self.job.cancel.clone()
    }
}

impl JobRegistry {
    /// Configure before recovery or starting producers. Zero limits opt out.
    pub fn configure_retention(&self, policy: Retention) -> Result<(), String> {
        policy.validate()?;
        let jobs = self.inner.jobs.lock().unwrap();
        if !jobs.is_empty() { return Err("configure retention before starting or recovering jobs".into()); }
        self.inner.budget.lock().unwrap().policy = policy;
        Ok(())
    }

    /// Expire only settled, delivered artifacts, never active or pending output.
    pub fn cleanup(&self) -> Result<usize, String> {
        let policy = self.inner.budget.lock().unwrap().policy.clone();
        if policy.max_age_secs == 0 { return Ok(0); }
        let now = retention::now_ms();
        let mut jobs = self.inner.jobs.lock().unwrap();
        let mut removed = Vec::new();
        let mut errors = Vec::new();
        for (id, job) in jobs.iter() {
            // The registry lock prevents acquisition of new readers. Existing
            // readers/producers must finish before either spool can disappear.
            if Arc::strong_count(job) > 1 { continue; }
            let state = job.state.lock().unwrap();
            if !state.settled || (!state.delivered && state.owner.is_some()) ||
                !state.settled_at_ms.is_some_and(|at| now.saturating_sub(at) >= policy.max_age_secs.saturating_mul(1000)) { continue; }
            if let Some(path) = &job.path {
                // Rename to a tombstone first. Recovery resumes interrupted GC.
                if let Err(error) = job.persist(&state) { tracing::warn!(%error, "artifact cleanup metadata restore failed"); }
                let tombstone = path.with_extension("deleted");
                match std::fs::rename(path, &tombstone) {
                    Ok(()) => {},
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                    Err(e) => { errors.push(e.to_string()); continue; },
                }
                if let Err(e) = sync_directory(path.parent().unwrap()) {
                    errors.push(e.to_string());
                    continue;
                }
                match std::fs::remove_file(path.with_extension("output")) {
                    Ok(()) => {},
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                    Err(e) => {
                        errors.push(e.to_string());
                        // Output still exists; restore metadata for readers.
                        if let Err(e) = std::fs::rename(&tombstone, path) { errors.push(e.to_string()); }
                        continue;
                    },
                }
                // Keep the tombstone if the output unlink cannot be synced.
                // The job must leave the registry once its output is removed.
                if let Err(e) = sync_directory(path.parent().unwrap()) {
                    errors.push(e.to_string());
                    removed.push((id.clone(), state.charged_bytes));
                    continue;
                }
                // A leftover tombstone is harmless and retried on recovery.
                if let Err(e) = std::fs::remove_file(tombstone) {
                    if e.kind() != std::io::ErrorKind::NotFound { errors.push(e.to_string()); }
                }
            }
            removed.push((id.clone(), state.charged_bytes));
        }
        for (id, bytes) in &removed {
            jobs.remove(id);
            let mut budget = self.inner.budget.lock().unwrap();
            budget.used = budget.used.saturating_sub(*bytes);
        }
        if errors.is_empty() { Ok(removed.len()) } else { Err(errors.join("; ")) }
    }

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
            // Reconcile interrupted deletion only while holding this owner's
            // exclusive lock. Never touch another live instance's artifacts.
            for file in std::fs::read_dir(entry.path()).map_err(|e| e.to_string())? {
                let path = file.map_err(|e| e.to_string())?.path();
                if path.extension().and_then(|s| s.to_str()) == Some("deleted") {
                    match std::fs::remove_file(path.with_extension("output")) {
                        Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                        Err(e) => return Err(e.to_string()),
                    }
                    // A checkpoint after failed rollback may have recreated
                    // metadata. The committed deletion wins over both copies.
                    match std::fs::remove_file(path.with_extension("json")) {
                        Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                        Err(e) => return Err(e.to_string()),
                    }
                    sync_directory(path.parent().unwrap()).map_err(|e| e.to_string())?;
                    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
                }
            }
            for file in std::fs::read_dir(entry.path()).map_err(|e| e.to_string())? {
                let path = file.map_err(|e| e.to_string())?.path();
                if path.extension().and_then(|s| s.to_str()) == Some("output") && !path.with_extension("json").exists() {
                    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
                let mut state: JobState = serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {e}",path.display()))?;
                use std::io::{Read, Seek, SeekFrom};
                let mut file = std::fs::File::open(path.with_extension("output")).map_err(|e| format!("job output: {e}"))?;
                state.output_bytes = usize::try_from(file.metadata().map_err(|e| e.to_string())?.len()).map_err(|e| e.to_string())?;
                file.seek(SeekFrom::Start(state.output_bytes.saturating_sub(MAX_READ_BYTES) as u64)).map_err(|e| e.to_string())?;
                file.take(MAX_READ_BYTES as u64).read_to_end(&mut state.output).map_err(|e| e.to_string())?;
                if state.read_from > state.output_bytes { return Err("invalid durable job output cursor".into()); }
                if !state.settled { state.status = JobStatus::Interrupted; state.settled = true; }
                let id = path.file_stem().unwrap().to_str().ok_or("invalid job ID")?.to_owned();
                state.settled_at_ms.get_or_insert_with(retention::now_ms);
                state.charged_bytes = state.output_bytes as u64;
                self.inner.budget.lock().unwrap().used += state.charged_bytes;
                let job = Arc::new(Job {budget:self.inner.budget.clone(),spool:Mutex::new(None),path:Some(path),state:Mutex::new(state),cancel:Default::default(),changed:Arc::new(tokio::sync::Notify::new()),completion:None});
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
                budget: Default::default(),
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
        let registry = Arc::downgrade(&self.inner);
        let period = self.inner.budget.lock().unwrap().policy.cleanup_interval_secs;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(period));
            loop {
                interval.tick().await;
                let Some(inner) = registry.upgrade() else { break; };
                if let Err(error) = (JobRegistry { inner }).cleanup() {
                    tracing::warn!(%error, "artifact cleanup failed");
                }
            }
        });
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
                        if state.settled && !state.delivered { state.owner.clone().map(|owner| (id.clone(),owner,job.clone(),format!("Job {id}: Background {} job finished {}. Read its output with job_output.",state.kind,state.marker()))) } else { None }
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

    /// Foreground captures reuse the job artifact and ownership model, without
    /// issuing a second completion notice or advertising a running background job.
    pub(crate) fn capture(&self, label: String, owner: Option<&String>) -> (String, JobWriter) {
        let (id, writer) = self.start_owned("bash-output", label, owner);
        {
            let mut state = writer.job.state.lock().unwrap();
            state.delivered = true;
            writer.job.checkpoint(&state);
        }
        (id, writer)
    }

    /// Register a new running job; returns its id and the producer handle.
    pub fn start(&self, kind: &'static str, label: String) -> (String, JobWriter) {
        self.start_owned(kind, label, None)
    }

    pub fn start_owned(&self, kind: &'static str, label: String, owner: Option<&String>) -> (String, JobWriter) {
        let id = if self.inner.directory.lock().unwrap().is_some() { format!("j{}",ulid::Ulid::new()) } else { format!("j{}", self.inner.next_id.fetch_add(1, Ordering::Relaxed)) };
        let ephemeral = self.inner.directory.lock().unwrap().is_none();
        let spool = if ephemeral { tempfile::tempfile().ok() } else { None };
        let spool_failed = ephemeral && spool.is_none();
        let job = Arc::new(Job {
            budget: self.inner.budget.clone(),
            spool: Mutex::new(spool),
            path: self.inner.directory.lock().unwrap().as_ref().map(|dir| dir.join(format!("{id}.json"))),
            state: Mutex::new(JobState {
                kind: kind.into(),
                owner: owner.cloned(),
                delivered: false,
                label,
                status: JobStatus::Running,
                output: Vec::new(),
                output_bytes: 0,
                charged_bytes: 0,
                read_from: 0,
                settled: false,
                settled_at_ms: None,
                output_error: None,
            }),
            completion: owner.map(|owner| (id.clone(), owner.clone(), self.inner.sessions.lock().expect("sessions lock").clone())),
            cancel: tokio_util::sync::CancellationToken::new(),
            changed: Arc::new(tokio::sync::Notify::new()),
        });
        if let Some(path) = &job.path {
            let created = std::fs::OpenOptions::new().create_new(true).write(true).open(path.with_extension("output")).and_then(|file| file.sync_all());
            if let Err(error) = created { job.cancel.cancel(); tracing::error!(%error,"job output creation failed"); }
        }
        if spool_failed { job.cancel.cancel(); }
        job.checkpoint(&job.state.lock().unwrap());
        self.inner.jobs.lock().expect("jobs lock").insert(id.clone(), job.clone());
        (id, JobWriter { job })
    }

    /// Count visible unsettled jobs without reading or copying their output.
    pub fn count(&self, session: &str) -> usize {
        self.inner.jobs.lock().expect("jobs lock").values().filter(|job| {
            let state = job.state.lock().expect("job lock");
            state.listable(Some(session)) && !state.settled
        }).count()
    }

    pub fn list(&self, session: &str) -> Vec<JobSnapshot> {
        let mut jobs: Vec<_> = self.inner.jobs.lock().expect("jobs lock").iter().filter_map(|(id, job)| {
            let state = job.state.lock().expect("job lock");
            state.listable(Some(session)).then(|| job.snapshot(id, &state))
        }).collect();
        jobs.sort_by(|a, b| a.job_id.len().cmp(&b.job_id.len()).then_with(|| a.job_id.cmp(&b.job_id)));
        jobs
    }

    /// Inspect a bounded output tail without advancing the model's read cursor.
    pub fn inspect(&self, session: &str, id: &str) -> Result<JobInspection, String> {
        let job = self.get_for_session(id, Some(session))?;
        let state = job.state.lock().expect("job lock");
        let output_bytes = state.output_bytes;
        let tail = &state.output[state.output.len().saturating_sub(MAX_INSPECT_BYTES)..];
        let mut output = String::from_utf8_lossy(tail).into_owned();
        // Lossy decoding can expand invalid bytes; retain a UTF-8-safe bounded tail.
        if output.len() > MAX_INSPECT_BYTES {
            let mut start = output.len() - MAX_INSPECT_BYTES;
            while !output.is_char_boundary(start) { start += 1; }
            output.drain(..start);
        }
        Ok(JobInspection { job: job.snapshot(id, &state), output, output_bytes })
    }

    /// Return whether this call made a new cancellation request, not settlement.
    pub fn stop(&self, session: &str, id: &str) -> Result<bool, String> {
        Ok(self.get_for_session(id, Some(session))?.request_stop())
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
mod inspection_tests {
    use super::*;

    #[test]
    fn visibility_and_cancellation_until_producer_settlement() {
        let jobs = JobRegistry::new();
        let (private, writer) = jobs.start_owned("bash", "private".into(), Some(&"owner".into()));
        let (shared, shared_writer) = jobs.start("bash", "shared".into());
        assert_eq!(jobs.count("owner"), 2);
        assert_eq!(jobs.count("foreign"), 1);
        assert_eq!(jobs.list("foreign")[0].job_id, shared);
        assert!(jobs.inspect("foreign", &private).is_err());
        assert!(jobs.stop("foreign", &private).is_err());
        assert!(!writer.cancelled().is_cancelled());
        assert!(jobs.inspect("owner", "missing").is_err());
        assert!(jobs.stop("owner", "missing").is_err());
        assert!(jobs.stop("owner", &private).unwrap());
        assert!(!jobs.stop("owner", &private).unwrap());
        let snapshot = jobs.inspect("owner", &private).unwrap().job;
        assert!(snapshot.running && snapshot.cancellation_requested);
        assert_eq!(snapshot.status, "cancelling");
        assert_eq!(jobs.count("owner"), 2);
        writer.settle(JobStatus::Exited(Some(0)));
        let snapshot = jobs.inspect("owner", &private).unwrap().job;
        assert!(!snapshot.running);
        assert!(snapshot.cancellation_requested);
        assert_eq!(snapshot.status, "killed");
        assert_eq!(jobs.count("owner"), 1);
        assert!(!jobs.stop("owner", &private).unwrap());
        shared_writer.settle(JobStatus::Exited(Some(0)));
        assert!(!jobs.stop("foreign", &shared).unwrap());
        assert!(!shared_writer.cancelled().is_cancelled());
        assert_eq!(jobs.count("owner"), 0);
        assert_eq!(jobs.list("owner").len(), 2);
    }

    #[test]
    fn quotas_cancel_without_silently_discarding_output() {
        let jobs = JobRegistry::new();
        jobs.configure_retention(Retention { max_job_bytes: 4, max_total_bytes: 6, ..Default::default() }).unwrap();
        let (_, first) = jobs.start("test", "one".into());
        first.append(b"1234");
        first.append(b"5");
        assert!(first.cancelled().is_cancelled());
        assert!(first.output_error().unwrap().contains("per-job"));
        assert_eq!(first.job.state.lock().unwrap().output_bytes, 4);
        let (_, second) = jobs.start("test", "two".into());
        second.append(b"12");
        second.append(b"3");
        assert!(second.cancelled().is_cancelled());
        assert!(second.output_error().unwrap().contains("total"));
    }

    #[test]
    fn cleanup_preserves_running_and_undelivered_artifacts_and_recovers_quota() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = JobRegistry::new();
        jobs.configure_retention(Retention { max_age_secs: 1, ..Default::default() }).unwrap();
        jobs.enable_persistence(dir.path()).unwrap();
        let (active, running) = jobs.start("test", "running".into());
        let (pending, undelivered) = jobs.start_owned("test", "pending".into(), Some(&"owner".into()));
        let (expired, complete) = jobs.capture("complete".into(), Some(&"owner".into()));
        for writer in [&running, &undelivered, &complete] { writer.append(b"123"); }
        undelivered.settle(JobStatus::Exited(Some(0)));
        complete.settle(JobStatus::Exited(Some(0)));
        for writer in [&running, &undelivered, &complete] { writer.job.state.lock().unwrap().settled_at_ms = Some(0); }
        let path = complete.job.path.clone().unwrap();
        assert_eq!(jobs.cleanup().unwrap(), 0, "reader/producer lease protects persistent artifacts");
        drop(complete);
        assert_eq!(jobs.cleanup().unwrap(), 1);
        assert!(jobs.get(&expired).is_err());
        assert!(jobs.get(&active).is_ok());
        assert!(jobs.get(&pending).is_ok());
        assert_eq!(jobs.inner.budget.lock().unwrap().used, 6);
        assert!(!path.exists());
        assert!(!path.with_extension("output").exists());
    }

    #[test]
    fn recovery_finishes_interrupted_cleanup_and_removes_orphan_spools() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = JobRegistry::new();
        jobs.enable_persistence(dir.path()).unwrap();
        let (id, writer) = jobs.start("test", "expired".into());
        writer.append(b"bytes");
        writer.settle(JobStatus::Exited(Some(0)));
        let path = writer.job.path.clone().unwrap();
        std::fs::rename(&path, path.with_extension("deleted")).unwrap();
        // A reader checkpoint after a failed cleanup rollback can recreate JSON.
        writer.job.checkpoint(&writer.job.state.lock().unwrap());
        let orphan = path.parent().unwrap().join("orphan.output");
        std::fs::write(&orphan, b"orphan").unwrap();
        drop(writer);
        drop(jobs);
        let jobs = JobRegistry::new();
        jobs.enable_persistence(dir.path()).unwrap();
        assert!(jobs.get(&id).is_err());
        assert!(!orphan.exists());
        assert!(!path.with_extension("deleted").exists());
        assert!(!path.with_extension("output").exists());
        assert_eq!(jobs.inner.budget.lock().unwrap().used, 0);
    }

    #[test]
    fn inspection_is_bounded_and_does_not_consume_output() {
        let jobs = JobRegistry::new();
        let (id, writer) = jobs.start("bash", "output".into());
        writer.append(b"first");
        let job = jobs.get(&id).unwrap();
        assert_eq!(drain_output(&job).0, "first");
        let bytes = vec![b'x'; MAX_INSPECT_BYTES + 100];
        writer.append(&bytes);
        for _ in 0..2 {
            let inspection = jobs.inspect("any", &id).unwrap();
            assert_eq!(inspection.output, "x".repeat(MAX_INSPECT_BYTES));
            assert_eq!(inspection.output_bytes, 5 + bytes.len());
            jobs.count("any");
            jobs.list("any");
            assert_eq!(job.state.lock().unwrap().read_from, 5);
        }
        assert_eq!(drain_output(&job).0, String::from_utf8(bytes).unwrap());
        assert!(!jobs.inspect("any", &id).unwrap().output.is_empty());
        writer.append(&vec![0xff; MAX_INSPECT_BYTES + 1]);
        let inspection = jobs.inspect("any", &id).unwrap();
        assert!(inspection.output.len() <= MAX_INSPECT_BYTES);
        assert!(inspection.output.ends_with('\u{fffd}'));
        writer.append("😀tail".as_bytes());
        assert!(jobs.inspect("any", &id).unwrap().output.ends_with("😀tail"));
    }

    #[tokio::test]
    async fn model_kill_and_user_stop_share_pending_cancellation() {
        let jobs = JobRegistry::new();
        let (id, writer) = jobs.start("bash", "shared cancellation".into());
        let tool = JobKillTool::new(jobs.clone());
        let result = tool.execute(json!({"job_id": id})).await.unwrap();
        assert!(result.contains("cancellation requested"));
        assert!(writer.cancelled().is_cancelled());
        assert!(!jobs.stop("any", &id).unwrap());
        assert_eq!(jobs.count("any"), 1);
        assert_eq!(jobs.inspect("any", &id).unwrap().job.status, "cancelling");
        writer.settle(JobStatus::Exited(Some(0)));
        assert_eq!(jobs.count("any"), 0);
        assert_eq!(jobs.inspect("any", &id).unwrap().job.status, "killed");
    }

    #[test]
    fn recovered_interrupted_jobs_are_not_active() {
        let directory = tempfile::tempdir().unwrap();
        let jobs = JobRegistry::new();
        jobs.enable_persistence(directory.path()).unwrap();
        let (id, writer) = jobs.start_owned("bash", "recover".into(), Some(&"owner".into()));
        writer.append(b"retained");
        drop(writer);
        drop(jobs);
        let jobs = JobRegistry::new();
        jobs.enable_persistence(directory.path()).unwrap();
        assert_eq!(jobs.count("owner"), 0);
        assert!(jobs.list("foreign").is_empty());
        let inspection = jobs.inspect("owner", &id).unwrap();
        assert_eq!(inspection.job.status, "interrupted");
        assert!(!inspection.job.running);
        assert_eq!(inspection.output, "retained");
        assert!(!jobs.stop("owner", &id).unwrap());
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
    #[tokio::test]
    async fn partial_output_failure_reconciles_pagination() {
        use std::io::Write;
        let registry = JobRegistry::new();
        let (id, writer) = registry.start("bash", "partial".into());
        writer.append(b"first");
        // Model a partial write followed by an I/O failure.
        writer.job.spool.lock().unwrap().as_mut().unwrap().write_all(b"partial").unwrap();
        {
            let mut state = writer.job.state.lock().unwrap();
            state.output_error = Some("I/O failure; output is incomplete".into());
            writer.job.reconcile_output(&mut state);
        }
        let tool = JobOutputTool::new(registry);
        let (text, metadata) = tool.read_presented(None, json!({"job_id":id,"offset":5})).await.unwrap();
        assert!(text.starts_with("partial\n"));
        assert_eq!(metadata["end_byte"], 12);
        let (text, _) = tool.read_presented(None, json!({"job_id":id,"offset":12})).await.unwrap();
        assert!(text.starts_with("\n[output bytes 12..12"));
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

    /// Foreground `Bash` capture artifacts (`kind: "bash-output"`) are not
    /// background jobs: they must not clutter `count`, `list`, or `job_list`,
    /// but stay fully readable by id via `job_output`.
    #[tokio::test]
    async fn bash_output_captures_are_hidden_from_listings_but_remain_readable() {
        let registry = JobRegistry::new();
        let owner = "owner".to_string();
        let (capture_id, capture) = registry.capture("cd x && grep -rn foo".into(), Some(&owner));
        capture.append(b"foo.rs:1:foo");
        capture.settle(JobStatus::Exited(Some(0)));
        let (bg_id, bg) = registry.start_owned("bash", "sleep 100".into(), Some(&owner));
        bg.append(b"still going");

        assert_eq!(registry.count(&owner), 1, "only the real background job should count");
        let listed = registry.list(&owner);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].job_id, bg_id);

        let list = JobListTool::new(registry.clone());
        let (text, metadata) = list.list_presented(Some(&owner)).unwrap();
        assert!(!text.contains("bash-output"), "{text}");
        assert!(text.contains("sleep 100"), "{text}");
        assert_eq!(metadata["total"], 1);

        // Still fully retrievable by id, just not listed.
        let output = JobOutputTool::new(registry);
        assert!(output.execute_in(&owner, json!({"job_id":capture_id})).await.unwrap().contains("foo.rs:1:foo"));
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
    let start = state.read_from.max(state.output_bytes.saturating_sub(state.output.len()));
    let skipped = start - state.read_from;
    let window = &state.output[start - (state.output_bytes - state.output.len())..];
    let mut text = String::from_utf8_lossy(window).into_owned();
    if skipped > 0 {
        text = format!("… {skipped} bytes skipped …\n{text}");
    }
    let presentation = json!({
        "version":1,"kind":"job_output","status":state.marker(),
        "start_byte":state.read_from + skipped,"end_byte":state.output_bytes,
        "skipped_bytes":skipped,"truncated":skipped > 0,
    });
    state.read_from = state.output_bytes;
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
                "offset": { "type": "integer", "minimum": 0, "description": "Read full retained output at this byte offset without advancing the incremental cursor; pages are at most 64 KiB" },
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
        if let Some(offset) = args.get("offset") {
            use std::io::{Read, Seek, SeekFrom};
            let offset = offset.as_u64().ok_or("offset must be a nonnegative byte offset")?;
            let state = job.state.lock().unwrap();
            if offset > state.output_bytes as u64 { return Err("offset exceeds retained output".into()); }
            let read = |file: &mut std::fs::File| -> Result<String, String> {
                file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
                let mut bytes = Vec::new();
                file.take((state.output_bytes as u64 - offset).min(MAX_READ_BYTES as u64)).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            };
            let text = if let Some(path) = &job.path {
                read(&mut std::fs::File::open(path.with_extension("output")).map_err(|e| e.to_string())?)?
            } else {
                read(job.spool.lock().unwrap().as_mut().ok_or("output spool unavailable")?)?
            };
            let end = (offset + MAX_READ_BYTES as u64).min(state.output_bytes as u64);
            let metadata = json!({"version":1,"kind":"job_output","job_id":id,"start_byte":offset,"end_byte":end,"total_bytes":state.output_bytes,"truncated":end < state.output_bytes as u64});
            return Ok((format!("{text}\n[output bytes {offset}..{end} of {}; next offset: {end}]\n{}", state.output_bytes, state.marker()), metadata));
        }
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
        if metadata["truncated"] == true {
            text.push_str(&format!("\nFull output: job_output(job_id=\"{id}\", offset=0); page using returned byte offsets."));
        }
        metadata["job_id"] = json!(id);
        if let Some(error) = &job.state.lock().unwrap().output_error {
            text.push_str(&format!("\n[output error: {error}]"));
            metadata["output_error"] = json!(error);
        }
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
                if !state.listable(session) { return None; }
                let n: u64 = id[1..].parse().unwrap_or(0);
                let record = json!({"job_id":id,"kind":state.kind,"status":state.marker(),"label":state.label});
                metadata_bytes += serde_json::to_vec(&record).unwrap().len();
                if metadata_bytes <= 48 * 1024 { records.push(record); }
                Some((n, format!("{id} [{}] {} — {}", state.kind, state.marker(), state.label)))
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
        if !job.request_stop() {
            let state = job.state.lock().expect("job lock");
            return Ok((format!("job {id} already finished {}", state.marker()), json!({"version":1,"kind":"job_kill","job_id":id,"cancellation_requested":false,"status":state.marker()})));
        }
        Ok((format!("cancellation requested for job {id}"), json!({"version":1,"kind":"job_kill","job_id":id,"cancellation_requested":true})))
    }
}
