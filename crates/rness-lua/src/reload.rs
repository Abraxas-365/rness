//! Hot reload: watch the plugin root, rebuild the VM on change.
//!
//! Strategy is swap-the-world, not patch-in-place: any *.lua change
//! under the root re-discovers all sources and loads them into a FRESH
//! VM (see [`crate::plugin_host::LuaHost::reload`]). No stale state, no
//! per-plugin disposer bookkeeping — the VM is the disposer.
//!
//! The engine's `Arc<ToolRegistry>` is re-synced after each swap so the
//! next turn advertises exactly the reloaded tool set.

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
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        let lua_change = event.paths.iter().any(|p| p.extension().is_some_and(|x| x == "lua"));
        if lua_change && !event.kind.is_access() {
            let _ = tx.send(());
        }
    })
    .map_err(|e| e.to_string())?;
    watcher
        .watch(&root, notify::RecursiveMode::Recursive)
        .map_err(|e| e.to_string())?;

    let task = tokio::spawn(async move {
        let mut lua_tools = initial_tools;
        while rx.recv().await.is_some() {
            // Debounce: editors fire bursts (write + rename + chmod).
            tokio::time::sleep(Duration::from_millis(150)).await;
            while rx.try_recv().is_ok() {}

            let sources = match crate::loader::discover(&root, &plugin_names) {
                Ok(s) => s,
                Err(e) => {
                    on_reload(ReloadReport::Failed(format!("discover: {e}")));
                    continue;
                }
            };
            let plugins = sources.len();
            match host.reload(sources).await {
                Ok(errors) => {
                    lua_tools =
                        crate::api::tools::sync_lua_tools(&registry, &host, &lua_tools).await;
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

    /// A broken rewrite still swaps (boot policy: skip broken plugins),
    /// but the error is reported and its tools drop out of the registry.
    #[tokio::test]
    async fn broken_rewrite_reports_error_and_drops_its_tools() {
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
        match report {
            ReloadReport::Reloaded { tools, errors, .. } => {
                assert!(tools.is_empty());
                assert_eq!(errors.len(), 1);
                assert_eq!(errors[0].0, "plugins/tool.lua");
            }
            ReloadReport::Failed(e) => panic!("expected skip-and-report, got: {e}"),
        }
        assert!(registry.names().is_empty());
    }
}
