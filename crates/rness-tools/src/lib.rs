//! Built-in tools, each a leaf plugin. Deleting this crate must still compile
//! the engine — tools register into seams, the engine defines them.
//!
//! Tool semantics: file-freshness tracking (Read before Edit),
//! unique-match edits, ignore-aware search.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rness_engine::tools::ToolRegistry;

pub mod sandbox;

pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod lsp;
pub mod jobs;
pub mod read;
pub mod skills;
pub mod subagent;
pub mod subagent_control;
pub mod write;
pub mod web;

/// The observed version of a file: mtime plus length. Comparing both
/// catches same-mtime rewrites that a timestamp alone would miss
/// (sub-second edits on coarse-mtime filesystems).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileVersion {
    mtime: SystemTime,
    len: u64,
}

impl FileVersion {
    fn of(meta: &std::fs::Metadata) -> Option<FileVersion> {
        Some(FileVersion { mtime: meta.modified().ok()?, len: meta.len() })
    }
}

/// Shared tool state: the working directory plus the freshness ledger.
///
/// Freshness invariant: Write/Edit on an EXISTING file require that the
/// file's CURRENT version (mtime + length) was observed by a Read (or
/// produced by our own Write/Edit) — the agent never blind-overwrites
/// content it hasn't seen.
pub struct Workspace {
    root: PathBuf,
    read_at: Mutex<HashMap<PathBuf, FileVersion>>,
    sessions: Mutex<HashMap<String, Arc<Workspace>>>,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self { root: root.into(), read_at: Mutex::new(HashMap::new()), sessions: Mutex::new(HashMap::new()) })
    }

    fn for_session(&self, session: &str, root: &Path) -> Arc<Self> {
        let mut sessions = self.sessions.lock().expect("workspace sessions lock");
        Arc::clone(sessions.entry(session.to_owned()).or_insert_with(|| Self::new(root)))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Absolute paths pass through; relative paths resolve against root.
    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        }
    }

    /// Record that the current content of `path` has been seen.
    fn mark_seen(&self, path: &Path) {
        if let Some(version) = std::fs::metadata(path).ok().as_ref().and_then(FileVersion::of) {
            self.read_at.lock().expect("freshness lock").insert(path.to_path_buf(), version);
        }
    }

    /// Fail if `path` exists but its current version hasn't been observed.
    fn ensure_fresh(&self, path: &Path) -> Result<(), String> {
        let Ok(meta) = std::fs::metadata(path) else {
            return Ok(()); // new file — nothing to be stale against
        };
        let current = FileVersion::of(&meta)
            .ok_or_else(|| format!("stat {}: no modification time", path.display()))?;
        match self.read_at.lock().expect("freshness lock").get(path) {
            None => Err(format!(
                "{} has not been read in this session — Read it before modifying",
                path.display()
            )),
            Some(seen) if current != *seen => Err(format!(
                "{} changed on disk since it was last read — Read it again first",
                path.display()
            )),
            Some(_) => Ok(()),
        }
    }
}

/// Register every built-in tool against one workspace. Returns the job
/// registry so later registrations (subagent — it needs the session
/// service, which mounts after tools) share the same job controls.
pub fn register_all(registry: &ToolRegistry, workspace: Arc<Workspace>) -> jobs::JobRegistry {
    let jobs = jobs::JobRegistry::new();
    registry.register(Arc::new(read::ReadTool::new(workspace.clone())));
    registry.register(Arc::new(write::WriteTool::new(workspace.clone())));
    registry.register(Arc::new(edit::EditTool::new(workspace.clone())));
    registry.register(Arc::new(glob::GlobTool::new(workspace.clone())));
    registry.register(Arc::new(grep::GrepTool::new(workspace.clone())));
    registry.register(Arc::new(bash::BashTool::new(workspace, jobs.clone())));
    registry.register(Arc::new(jobs::JobOutputTool::new(jobs.clone())));
    registry.register(Arc::new(jobs::JobListTool::new(jobs.clone())));
    registry.register(Arc::new(jobs::JobKillTool::new(jobs.clone())));
    jobs
}

/// Register the delegation tool. Separate from [`register_all`] because
/// the subagent runtime wraps the session service, which is composed
/// AFTER the tool registry (the registry is interior-mutable precisely
/// for late registrations like this one).
pub fn register_subagent(
    registry: &ToolRegistry,
    runtime: Arc<rness_engine::subagent::SubagentRuntime>,
    jobs: jobs::JobRegistry,
) {
    registry.register(Arc::new(subagent::SubagentTool::new(runtime, jobs)));
}

/// Pull a required string argument out of tool args.
fn required_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing required argument '{key}'"))
}
