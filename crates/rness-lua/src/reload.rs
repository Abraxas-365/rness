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
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
    let runtime_root = root.join("plugins");

    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        let lua_change = event.paths.iter().any(|p| p.extension().is_some_and(|x| x == "lua"));
        if lua_change && !event.kind.is_access() {
            let startup = event.paths.iter().any(|p| p.extension().is_some_and(|x| x == "lua") && !p.starts_with(&runtime_root));
            let _ = tx.send(startup);
        }
    })
    .map_err(|e| e.to_string())?;
    watcher
        .watch(&root, notify::RecursiveMode::Recursive)
        .map_err(|e| e.to_string())?;

    let task = tokio::spawn(async move {
        let mut lua_tools = initial_tools;
        while let Some(mut startup_changed) = rx.recv().await {
            // Debounce: editors fire bursts (write + rename + chmod).
            tokio::time::sleep(Duration::from_millis(150)).await;
            while let Ok(startup) = rx.try_recv() { startup_changed |= startup; }
            if startup_changed {
                on_reload(ReloadReport::Failed("startup Lua changed; restart rness to apply init.lua or module changes".into()));
                continue;
            }

            let sources = match crate::loader::discover(&root, &plugin_names) {
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
                Err(e) => on_reload(ReloadReport::Failed(e)),
            }
        }
    });

    Ok(ReloadWatcher { _watcher: watcher, task })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
