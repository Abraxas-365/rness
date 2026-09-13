//! Watch selected runtime plugins and reconcile registrations without rerunning startup.
//! Failed reloads preserve the previous registration set; arbitrary Lua side effects
//! and module caches are not rolled back.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::Watcher as _;

use crate::plugin_host::LuaHost;

/// Owns the OS watcher and the reload task; dropping it stops both.
pub struct ReloadWatcher {
    _watcher: notify::RecommendedWatcher,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ReloadWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Outcome of one reload pass, for whoever wants to surface it.
#[derive(Debug, Clone)]
pub enum ReloadReport {
    /// VM swapped. Per-plugin load errors (skipped files) included.
    Reloaded { plugins: usize, tools: Vec<String>, errors: Vec<(String, String)> },
    /// Reload failed outright; the previous VM is still live.
    Failed(String),
}

/// Watch `root` for `*.lua` changes and hot-swap the VM.
/// `initial_tools` is what the boot-time
/// registration produced (so the first sync can unregister removed
/// tools). `on_reload` runs after every pass.
pub fn watch(
    root: PathBuf,
    plugin_names: Vec<String>,
    host: LuaHost,
    registry: Arc<rness_engine::tools::ToolRegistry>,
    initial_tools: crate::api::tools::InstalledTools,
    on_reload: impl Fn(ReloadReport) + Send + 'static,
) -> Result<ReloadWatcher, String> {
    watch_selected(root, plugin_names, Vec::new(), host, registry, initial_tools, on_reload)
}

/// Select the watcher matching the startup declaration format.
pub fn watch_startup(
    root: PathBuf,
    startup: &crate::api::config::StartupConfig,
    host: LuaHost,
    registry: Arc<rness_engine::tools::ToolRegistry>,
    initial_tools: crate::api::tools::InstalledTools,
    on_reload: impl Fn(ReloadReport) + Send + 'static,
) -> Result<ReloadWatcher, String> {
    watch_selected(root, startup.plugins.clone(), startup.plugin_specs.clone(), host, registry, initial_tools, on_reload)
}

/// Watch explicit development sources; retain non-watched source snapshots.
pub fn watch_specs(
    root: PathBuf,
    specs: Vec<crate::loader::PluginSpec>,
    host: LuaHost,
    registry: Arc<rness_engine::tools::ToolRegistry>,
    initial_tools: crate::api::tools::InstalledTools,
    on_reload: impl Fn(ReloadReport) + Send + 'static,
) -> Result<ReloadWatcher, String> {
    watch_selected(root, Vec::new(), specs, host, registry, initial_tools, on_reload)
}

fn watch_selected(
    root: PathBuf,
    plugin_names: Vec<String>,
    specs: Vec<crate::loader::PluginSpec>,
    host: LuaHost,
    registry: Arc<rness_engine::tools::ToolRegistry>,
    initial_tools: crate::api::tools::InstalledTools,
    on_reload: impl Fn(ReloadReport) + Send + 'static,
) -> Result<ReloadWatcher, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
    let runtime_root = root.join("plugins");
    let explicit = !specs.is_empty();
    let snapshots = crate::loader::discover_specs(&root, &specs).map_err(|e| e.to_string())?;
    let mut watched_files = Vec::new();
    let mut watched_directories = Vec::new();
    for spec in specs.iter().filter(|s| s.enabled && s.watch) {
        match &spec.source {
            crate::loader::PluginLocation::File(path) => {
                let path = root.join(path).canonicalize().map_err(|e| e.to_string())?;
                watched_files.push(path);
            }
            crate::loader::PluginLocation::Package(name) => {
                let inventory = crate::packages::inventory(&root).map_err(|e| e.to_string())?;
                let package = inventory.get(name).ok_or_else(|| format!("missing package {name}"))?;
                watched_directories.push(package.directory.canonicalize().map_err(|e| e.to_string())?);
            }
            crate::loader::PluginLocation::Inline => {}
        }
    }
    let mut watch_roots = vec![root.clone()];
    for path in &watched_files {
        if let Some(parent) = path.parent() {
            if !parent.starts_with(&root) { watch_roots.push(parent.to_path_buf()); }
        }
    }
    watch_roots.extend(watched_directories.iter().filter(|p| !p.starts_with(&root)).cloned());
    watch_roots.sort();
    watch_roots.dedup();
    let startup_file = root.join("init.lua");
    let startup_modules = root.join("lua");

    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        let source_change = event.paths.iter().any(|p| {
            p.extension().is_some_and(|x| x == "lua")
                || (explicit && (watched_files.contains(p)
                    || (p.file_name().is_some_and(|name| name == "rness-plugin.json")
                        && watched_directories.iter().any(|dir| p.starts_with(dir)))))
        });
        if source_change && !event.kind.is_access() {
            let startup = if explicit {
                event.paths.iter().any(|p| p == &startup_file || p.starts_with(&startup_modules))
            } else {
                event.paths.iter().any(|p| p.extension().is_some_and(|x| x == "lua") && !p.starts_with(&runtime_root))
            };
            let selected = !explicit || event.paths.iter().any(|p| watched_files.contains(p) || watched_directories.iter().any(|dir| p.starts_with(dir)));
            if startup || selected { let _ = tx.send(startup); }
        }
    })
    .map_err(|e| e.to_string())?;
    for path in watch_roots {
        watcher.watch(&path, notify::RecursiveMode::Recursive).map_err(|e| e.to_string())?;
    }

    let task = tokio::spawn(async move {
        let mut lua_tools = initial_tools;
        let mut retry_pending = false;
        loop {
            let mut startup_changed = if retry_pending {
                tokio::select! {
                    event = rx.recv() => match event { Some(startup) => startup, None => break },
                    _ = tokio::time::sleep(Duration::from_millis(150)) => false,
                }
            } else {
                match rx.recv().await { Some(startup) => startup, None => break }
            };
            // Debounce: editors fire bursts (write + rename + chmod).
            tokio::time::sleep(Duration::from_millis(150)).await;
            while let Ok(startup) = rx.try_recv() { startup_changed |= startup; }
            if startup_changed {
                retry_pending = false;
                on_reload(ReloadReport::Failed("startup Lua changed; restart rness to apply init.lua or module changes".into()));
                continue;
            }

            retry_pending = false;
            let discovered = if explicit {
                let mut sources = snapshots.clone();
                // Startup discovery validated the full graph. Refresh only watched
                // sources without requiring their unwatched dependencies in this subset.
                let watched: Vec<_> = specs.iter().filter(|s| s.watch).cloned().map(|mut spec| {
                    spec.dependencies.clear();
                    spec
                }).collect();
                let refreshed = crate::loader::discover_specs(&root, &watched);
                refreshed.map(|refreshed| {
                    for source in refreshed {
                        if let Some(slot) = sources.iter_mut().find(|s| s.name == source.name) { slot.source = source.source; }
                    }
                    sources
                })
            } else { crate::loader::discover(&root, &plugin_names) };
            let sources = match discovered {
                Ok(s) => s,
                Err(e) => {
                    on_reload(ReloadReport::Failed(format!("discover: {e}")));
                    continue;
                }
            };
            let plugins = sources.len();
            let (synced, receive) = tokio::sync::oneshot::channel();
            let registry = registry.clone();
            let previous = lua_tools.clone();
            let adapter = host.clone();
            match host.reload_reconciled(sources, move |specs| {
                let tools = crate::api::tools::sync_lua_tool_specs(&registry, &adapter, &previous, specs);
                let _ = synced.send(tools);
            }).await {
                Ok(errors) => {
                    lua_tools = receive.await.expect("reload reconciliation completed");
                    on_reload(ReloadReport::Reloaded {
                        plugins,
                        tools: lua_tools.iter().map(|tool| tool.name().to_owned()).collect(),
                        errors,
                    });
                }
                Err(crate::plugin_host::ReloadError::Busy) => retry_pending = true,
                Err(error) => on_reload(ReloadReport::Failed(error.to_string())),
            }
        }
    });

    Ok(ReloadWatcher { _watcher: watcher, task })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn explicit_external_file_reload_preserves_unwatched_snapshot() {
        let config = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let live = external.path().join("live.lua");
        std::fs::write(&live, "rness.tool.register{name='live', run=function() return 'old' end}").unwrap();
        let fixed = config.path().join("fixed.lua");
        std::fs::write(&fixed, "rness.tool.register{name='fixed', run=function() return 'fixed' end}").unwrap();
        let spec = |name: &str, path: PathBuf, watch| crate::loader::PluginSpec {
            dependencies: vec![],
            name: name.into(), source: crate::loader::PluginLocation::File(path), enabled: true,
            watch, opts: serde_json::json!({}), keys: serde_json::json!({}),
        };
        let mut live_spec = spec("live", live.clone(), true);
        live_spec.dependencies = vec!["fixed".into()];
        let specs = vec![live_spec, spec("fixed", fixed.clone(), false)];
        let host = LuaHost::spawn().unwrap();
        let sources = crate::loader::discover_specs(config.path(), &specs).unwrap();
        assert!(crate::loader::load_all(&host, &sources).await.is_empty());
        let registry = Arc::new(rness_engine::tools::ToolRegistry::default());
        let installed = crate::api::tools::sync_lua_tools(&registry, &host, &[]).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _watch = watch_specs(config.path().into(), specs, host.clone(), registry.clone(), installed, move |report| { let _ = tx.send(report); }).unwrap();
        std::fs::write(&fixed, "error('unwatched source must not be re-read')").unwrap();
        let replacement = external.path().join("replacement.lua");
        std::fs::write(&replacement, "rness.tool.register{name='replacement', run=function() return 'new' end}").unwrap();
        std::fs::rename(replacement, live).unwrap();
        let report = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(report, ReloadReport::Reloaded { ref errors, .. } if errors.is_empty()), "{report:?}");
        assert!(registry.get("replacement").is_some());
        assert!(registry.get("live").is_none());
        assert!(registry.get("fixed").is_some());
        let error = host.unload("fixed").await.unwrap_err();
        assert!(error.contains("required by: live"), "{error}");
    }

    /// End to end: boot with one tool, rewrite the file, watcher swaps
    /// the VM and re-syncs the engine registry.
    #[tokio::test]
    async fn file_change_swaps_vm_and_resyncs_registry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join("plugins")).unwrap();
        std::fs::write(
            root.join("plugins/a.lua"),
            r#"rness.tool.register{ name = "old_tool", run = function() return "v1" end }"#,
        )
        .unwrap();

        let plugin_names = vec!["a".into()];
        let host = LuaHost::spawn().unwrap();
        let sources = crate::loader::discover(&root, &plugin_names).unwrap();
        assert!(crate::loader::load_all(&host, &sources).await.is_empty());

        let registry = Arc::new(rness_engine::tools::ToolRegistry::default());
        crate::api::tools::register_lua_tools(&registry, &host).await;
        assert_eq!(registry.names(), vec!["old_tool"]);

        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
        let _watcher = watch(
            root.clone(),
            plugin_names,
            host.clone(),
            Arc::clone(&registry),
            vec![registry.get("old_tool").unwrap()],
            move |report| {
                let _ = done_tx.send(report);
            },
        )
        .unwrap();

        // Rename the tool on disk.
        std::fs::write(
            root.join("plugins/a.lua"),
            r#"rness.tool.register{ name = "new_tool", run = function() return "v2" end }"#,
        )
        .unwrap();

        let report = tokio::time::timeout(Duration::from_secs(10), done_rx.recv())
            .await
            .expect("watcher fired")
            .expect("report");
        match report {
            ReloadReport::Reloaded { tools, errors, .. } => {
                assert_eq!(tools, vec!["new_tool"]);
                assert!(errors.is_empty());
            }
            ReloadReport::Failed(e) => panic!("reload failed: {e}"),
        }

        // Old tool gone from the ENGINE registry, new one callable.
        assert_eq!(registry.names(), vec!["new_tool"]);
        assert_eq!(host.call_tool("new_tool", json!({})).await, Ok("v2".into()));
    }

    /// A broken rewrite preserves the previous tool registrations.
    #[tokio::test]
    async fn broken_rewrite_preserves_its_tools() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("plugins")).unwrap();
        std::fs::write(
            root.join("plugins/tool.lua"),
            r#"rness.tool.register{ name = "t", run = function() return "ok" end }"#,
        )
        .unwrap();

        let plugin_names = vec!["tool".into()];
        let host = LuaHost::spawn().unwrap();
        let sources = crate::loader::discover(&root, &plugin_names).unwrap();
        crate::loader::load_all(&host, &sources).await;
        let registry = Arc::new(rness_engine::tools::ToolRegistry::default());
        crate::api::tools::register_lua_tools(&registry, &host).await;

        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
        let _watcher = watch(
            root.clone(),
            plugin_names,
            host.clone(),
            Arc::clone(&registry),
            vec![registry.get("t").unwrap()],
            move |report| {
                let _ = done_tx.send(report);
            },
        )
        .unwrap();

        std::fs::write(root.join("plugins/tool.lua"), "this is not lua").unwrap();

        let report = tokio::time::timeout(Duration::from_secs(10), done_rx.recv())
            .await
            .expect("watcher fired")
            .expect("report");
        assert!(matches!(report, ReloadReport::Failed(_)));
        assert_eq!(registry.names(), vec!["t"]);
        assert_eq!(host.call_tool("t", json!({})).await.unwrap(), "ok");
    }
}
