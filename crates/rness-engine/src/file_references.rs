//! Shared bounded path index. Contents are never attached or read.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub const GUIDANCE: &str = "Tokens prefixed with @ are user-referenced paths (workspace-relative, ../, ~/, or absolute), not attached file contents. A trailing slash denotes a directory: list it when needed. Otherwise use the read tool when contents are needed. Never claim to have inspected a reference before reading it. @\"...\" quotes paths containing spaces.";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub max_results: usize,
    pub max_entries: usize,
    pub excluded_directories: Vec<String>,
    pub respect_gitignore: bool,
    pub allow_parent: bool,
    pub allow_home: bool,
    pub allow_absolute: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_results: 20,
            max_entries: 50_000,
            excluded_directories: [
                ".git",
                "node_modules",
                "dist",
                "build",
                "out",
                "coverage",
                "target",
                ".next",
                ".nuxt",
                ".turbo",
                ".venv",
                "__pycache__",
                ".pytest_cache",
                ".mypy_cache",
                ".gradle",
            ]
            .map(String::from)
            .to_vec(),
            respect_gitignore: true,
            allow_parent: false,
            allow_home: false,
            allow_absolute: false,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=200).contains(&self.max_results) || !(1..=1_000_000).contains(&self.max_entries) {
            return Err("invalid reference limits: results 1..200, entries 1..1000000".into());
        }
        if self
            .excluded_directories
            .iter()
            .any(|s| s.is_empty() || s.contains(['/', '\\']) || s == "." || s == "..")
        {
            return Err("excluded_directories must contain directory basenames".into());
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
fn default_limit() -> usize {
    200
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub path: String,
    pub directory: bool,
}
#[derive(Default)]
struct Index {
    entries: Option<Arc<Vec<Candidate>>>,
    building: bool,
    version: u64,
    built: u64,
    settled: Option<Instant>,
    error: Option<String>,
}
struct Workspace {
    index: Mutex<Index>,
    ready: std::sync::Condvar,
    cancel: CancellationToken,
}
impl Default for Workspace {
    fn default() -> Self {
        Self {
            index: Mutex::new(Index::default()),
            ready: std::sync::Condvar::new(),
            cancel: CancellationToken::new(),
        }
    }
}
#[derive(Default)]
struct State {
    config: Option<Config>,
    workspaces: HashMap<PathBuf, Arc<Workspace>>,
    generation: u64,
}
#[derive(Default)]
pub struct FileReferences {
    state: Mutex<State>,
}
impl FileReferences {
    pub fn configure(&self, config: Option<Config>) {
        let mut state = self.state.lock().unwrap();
        for workspace in state.workspaces.values() {
            workspace.cancel.cancel();
            workspace.ready.notify_all();
        }
        state.workspaces.clear();
        state.config = config;
        state.generation += 1;
    }
    pub fn generation(&self) -> u64 {
        self.state.lock().unwrap().generation
    }
    pub fn enabled(&self) -> bool {
        self.state.lock().unwrap().config.is_some()
    }
    pub fn invalidate(&self) {
        for workspace in self.state.lock().unwrap().workspaces.values() {
            workspace.index.lock().unwrap().version += 1;
        }
    }
    pub fn list(
        &self,
        root: &Path,
        query: &Query,
        cancel: &CancellationToken,
    ) -> Result<Vec<Candidate>, String> {
        if query.limit == 0
            || query.limit > 200
            || query.query.len() > 4096
            || query.query.chars().any(char::is_control)
        {
            return Err("invalid reference query or limit".into());
        }
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        let (config, workspace) = {
            let mut state = self.state.lock().unwrap();
            let Some(config) = state.config.clone() else {
                return Ok(Vec::new());
            };
            if !state.workspaces.contains_key(&root) && state.workspaces.len() >= 8 {
                let old = state.workspaces.keys().next().cloned().unwrap();
                state.workspaces.remove(&old).unwrap().cancel.cancel();
            }
            (
                config,
                state.workspaces.entry(root.clone()).or_default().clone(),
            )
        };
        let home = query.query.starts_with("~/");
        let absolute = Path::new(&query.query).is_absolute();
        let parent = Path::new(&query.query)
            .components()
            .any(|c| c == Component::ParentDir);
        if (home && !config.allow_home)
            || (absolute && !config.allow_absolute)
            || (parent && !config.allow_parent)
        {
            return Err("external reference path is disabled by Lua configuration".into());
        }
        let limit = query.limit.min(config.max_results);
        if home || absolute || parent {
            let (directory, fragment) = query.query.rsplit_once('/').unwrap_or((&query.query, ""));
            let base = if home {
                PathBuf::from(std::env::var_os("HOME").ok_or("home directory is unavailable")?)
                    .canonicalize()
                    .map_err(|e| e.to_string())?
            } else if absolute {
                PathBuf::from("/")
            } else {
                root.clone()
            };
            let relative = if home {
                directory.strip_prefix('~').unwrap().trim_start_matches('/')
            } else {
                directory
            };
            let mut dir = base;
            for component in Path::new(relative).components() {
                dir.push(component.as_os_str());
                if std::fs::symlink_metadata(&dir)
                    .map_err(|e| e.to_string())?
                    .file_type()
                    .is_symlink()
                {
                    return Err("directory symlinks are not traversed".into());
                }
            }
            let dir = dir.canonicalize().map_err(|e| e.to_string())?;
            if dir.components().any(|c| {
                config
                    .excluded_directories
                    .iter()
                    .any(|excluded| c.as_os_str() == excluded.as_str())
            }) {
                return Ok(Vec::new());
            }
            let entries = scan(&dir, &dir, &config, Some(1), cancel, &workspace.cancel)?;
            let mut result = rank(&entries, fragment, limit, true);
            let prefix = if query.query.contains('/') {
                format!("{directory}/")
            } else {
                format!("{}/", query.query)
            };
            for entry in &mut result {
                entry.path = format!("{prefix}{}", entry.path);
            }
            if cancel.is_cancelled() || workspace.cancel.is_cancelled() {
                result.clear();
            }
            return Ok(result);
        }
        if query.query.is_empty() || query.query.contains('/') {
            let (directory, fragment) = query.query.rsplit_once('/').unwrap_or(("", ""));
            let mut dir = root.clone();
            for component in Path::new(directory).components() {
                dir.push(component.as_os_str());
                if std::fs::symlink_metadata(&dir)
                    .map_err(|e| e.to_string())?
                    .file_type()
                    .is_symlink()
                {
                    return Err("directory symlinks are not traversed".into());
                }
            }
            let entries = scan(&root, &dir, &config, Some(1), cancel, &workspace.cancel)?;
            let mut result = rank(&entries, fragment, limit, true);
            if cancel.is_cancelled() || workspace.cancel.is_cancelled() {
                result.clear();
            }
            return Ok(result);
        }
        let mut index = workspace.index.lock().unwrap();
        let stale = index.entries.is_none()
            || index.built != index.version
            || index
                .settled
                .is_some_and(|t| t.elapsed() > Duration::from_secs(30));
        if stale && !index.building {
            index.building = true;
            index.error = None;
            let version = index.version;
            let worker = workspace.clone();
            let config = config.clone();
            std::thread::spawn(move || {
                let result = scan(&root, &root, &config, None, &worker.cancel, &worker.cancel);
                let mut index = worker.index.lock().unwrap();
                match result {
                    Ok(entries) => {
                        index.entries = Some(Arc::new(entries));
                        index.built = version;
                        index.settled = Some(Instant::now());
                    }
                    Err(e) => index.error = Some(e),
                }
                index.building = false;
                worker.ready.notify_all();
            });
        }
        while index.entries.is_none() && index.building {
            if cancel.is_cancelled() || workspace.cancel.is_cancelled() {
                return Err("file lookup cancelled".into());
            }
            index = workspace
                .ready
                .wait_timeout(index, Duration::from_millis(25))
                .unwrap()
                .0;
        }
        if cancel.is_cancelled() || workspace.cancel.is_cancelled() {
            return Err("file lookup cancelled".into());
        }
        let entries = index.entries.clone().ok_or_else(|| {
            index
                .error
                .clone()
                .unwrap_or_else(|| "index unavailable".into())
        })?;
        drop(index);
        Ok(rank(&entries, &query.query, limit, false))
    }
}
impl Drop for FileReferences {
    fn drop(&mut self) {
        for workspace in self.state.get_mut().unwrap().workspaces.values() {
            workspace.cancel.cancel();
        }
    }
}
fn scan(
    root: &Path,
    dir: &Path,
    config: &Config,
    depth: Option<usize>,
    caller: &CancellationToken,
    lifetime: &CancellationToken,
) -> Result<Vec<Candidate>, String> {
    if dir.strip_prefix(root).unwrap_or(dir).components().any(|c| {
        config
            .excluded_directories
            .iter()
            .any(|excluded| c.as_os_str() == excluded.as_str())
    }) {
        return Ok(Vec::new());
    }
    let exclusions = config.excluded_directories.clone();
    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .hidden(false)
        .follow_links(false)
        .max_depth(depth)
        .git_ignore(config.respect_gitignore)
        .git_exclude(config.respect_gitignore)
        .git_global(config.respect_gitignore)
        .ignore(config.respect_gitignore)
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b));
    builder.filter_entry(move |entry| {
        !(entry.file_type().is_some_and(|t| t.is_dir())
            && exclusions.iter().any(|s| entry.file_name() == s.as_str()))
    });
    let mut entries = Vec::new();
    for entry in builder.build() {
        if caller.is_cancelled() || lifetime.is_cancelled() {
            return Err("file lookup cancelled".into());
        }
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.depth() == 0 {
            continue;
        }
        let Some(path) = entry.path().strip_prefix(root).ok().and_then(Path::to_str) else {
            continue;
        };
        if path.chars().any(|c| c.is_control() || c == '"') {
            continue;
        }
        let directory = entry.file_type().is_some_and(|t| t.is_dir());
        entries.push(Candidate {
            path: format!("{path}{}", if directory { "/" } else { "" }),
            directory,
        });
        if entries.len() >= config.max_entries {
            break;
        }
    }
    Ok(entries)
}
fn rank(entries: &[Candidate], query: &str, limit: usize, basename: bool) -> Vec<Candidate> {
    let needle = query.to_lowercase();
    let mut scored: Vec<_> = entries
        .iter()
        .filter_map(|candidate| {
            let path = candidate.path.trim_end_matches('/').to_lowercase();
            let target = if basename {
                path.rsplit('/').next().unwrap_or(&path)
            } else {
                &path
            };
            let mut cursor = 0;
            let mut gaps = 0;
            for character in needle.chars() {
                let offset = target[cursor..].find(character)?;
                gaps += offset;
                cursor += offset + character.len_utf8();
            }
            Some((
                (!target.starts_with(&needle), gaps, candidate.path.len()),
                candidate,
            ))
        })
        .collect();
    scored.sort_by(|(a, ac), (b, bc)| a.cmp(b).then(ac.path.cmp(&bc.path)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, c)| c.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn external_paths_require_opt_in_and_browse_one_level() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(tmp.path().join("outside.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        std::fs::write(tmp.path().join("nested/deep.txt"), "").unwrap();
        let service = FileReferences::default();
        service.configure(Some(Config::default()));
        let cancel = CancellationToken::new();
        let query = |s: &str| Query {
            query: s.into(),
            limit: 20,
        };
        for text in ["../", "~/", "/"] {
            assert!(service.list(&root, &query(text), &cancel).is_err());
        }
        service.configure(Some(Config {
            allow_parent: true,
            ..Default::default()
        }));
        let entries = service.list(&root, &query("../"), &cancel).unwrap();
        assert!(entries.iter().any(|c| c.path == "../outside.txt"));
        assert!(entries.iter().any(|c| c.path == "../nested/"));
        assert!(!entries.iter().any(|c| c.path.contains("deep.txt")));
        assert!(service.list(&root, &query("~/"), &cancel).is_err());
        service.configure(Some(Config {
            allow_absolute: true,
            ..Default::default()
        }));
        let directory = tmp.path().canonicalize().unwrap();
        let text = format!("{}/out", directory.display());
        let entries = service.list(&root, &query(&text), &cancel).unwrap();
        assert_eq!(
            entries[0].path,
            format!("{}/outside.txt", directory.display())
        );
        assert!(service.list(&root, &query("../"), &cancel).is_err());
        let config: Config = serde_json::from_str(r#"{"allow_home":true}"#).unwrap();
        assert!(config.allow_home);
        assert!(!config.allow_parent);
        service.configure(Some(config));
        if std::env::var_os("HOME").is_some() {
            let entries = service.list(&root, &query("~/"), &cancel).unwrap();
            assert!(entries
                .iter()
                .all(|c| c.path.starts_with("~/")
                    && !c.path[2..].trim_end_matches('/').contains('/')));
            assert!(service.list(&root, &query("~/../"), &cancel).is_err());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(tmp.path().join("nested"), root.join("link")).unwrap();
            service.configure(Some(Config {
                allow_parent: true,
                ..Default::default()
            }));
            assert!(service.list(&root, &query("link/../"), &cancel).is_err());
        }
    }

    #[test]
    fn shared_cache_refresh_and_entry_budget() {
        let root = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta", "gamma"] {
            std::fs::write(root.path().join(name), "").unwrap();
        }
        let service = FileReferences::default();
        service.configure(Some(Config {
            max_entries: 2,
            ..Default::default()
        }));
        let token = CancellationToken::new();
        let query = Query {
            query: "a".into(),
            limit: 200,
        };
        assert_eq!(service.list(root.path(), &query, &token).unwrap().len(), 2);
        let workspace = service
            .state
            .lock()
            .unwrap()
            .workspaces
            .values()
            .next()
            .unwrap()
            .clone();
        let before = workspace.index.lock().unwrap().entries.clone().unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(service.list(root.path(), &query, &cancelled).is_err());
        assert!(Arc::ptr_eq(
            &before,
            workspace.index.lock().unwrap().entries.as_ref().unwrap()
        ));
        std::fs::remove_file(root.path().join("alpha")).unwrap();
        service.invalidate();
        service.list(root.path(), &query, &token).unwrap();
        let mut index = workspace.index.lock().unwrap();
        while index.building {
            index = workspace.ready.wait(index).unwrap();
        }
        assert!(!index
            .entries
            .as_ref()
            .unwrap()
            .iter()
            .any(|c| c.path == "alpha"));
        assert_eq!(index.version, index.built);
        drop(index);
        service.configure(None);
        assert!(workspace.cancel.is_cancelled());
    }

    #[test]
    fn live_directories_fuzzy_ignore_limits_and_disposal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "not attached").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "hidden.txt\n").unwrap();
        std::fs::write(dir.path().join("hidden.txt"), "ignored").unwrap();
        let service = FileReferences::default();
        service.configure(Some(Config::default()));
        let token = CancellationToken::new();
        let query = |s: &str| Query {
            query: s.into(),
            limit: 200,
        };
        let result = service.list(dir.path(), &query("src/"), &token).unwrap();
        assert_eq!(result[0].path, "src/main.rs");
        assert_eq!(
            service.list(dir.path(), &query("smrs"), &token).unwrap()[0].path,
            "src/main.rs"
        );
        assert!(service
            .list(dir.path(), &query("hidden"), &token)
            .unwrap()
            .is_empty());
        assert!(service.list(dir.path(), &query("../"), &token).is_err());
        service.configure(None);
        assert!(service
            .list(dir.path(), &query("smrs"), &token)
            .unwrap()
            .is_empty());
        service.configure(Some(Config {
            respect_gitignore: false,
            ..Default::default()
        }));
        assert_eq!(
            service
                .list(dir.path(), &query("hidden"), &token)
                .unwrap()
                .len(),
            1
        );
    }
}
