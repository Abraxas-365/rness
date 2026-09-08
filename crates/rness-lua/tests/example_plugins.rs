//! The example runtime/ plugins must actually load and register their
//! apps — this is the M5 dogfood check in CI form. (They are NOT
//! embedded or auto-loaded; users copy them into ~/.rness/plugins.)

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::EventBus;
use rness_lua::plugin_host::LuaHost;
use rness_protocol::events::*;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn custom_tools_receive_session_workspace_without_chdir() {
    let host = LuaHost::spawn().unwrap();
    host.load("context", "rness.tool.register{name='where', run=function(args, ctx) return ctx.session .. ':' .. ctx.workspace end}").await.unwrap();
    let registry = ToolRegistry::default();
    rness_lua::api::tools::sync_lua_tools(&registry, &host, &[]).await;
    let a = registry.for_workspace(&"a".into(), std::path::Path::new("/project-a"));
    let b = registry.for_workspace(&"b".into(), std::path::Path::new("/project-b"));
    assert_eq!(a.get("where").unwrap().execute(serde_json::json!({})).await.unwrap(), "a:/project-a");
    assert_eq!(b.get("where").unwrap().execute(serde_json::json!({})).await.unwrap(), "b:/project-b");
}

#[test]
fn command_example_registers_help_and_arguments() {
    let mut runtime = rness_lua::runtime::LuaRuntime::new().unwrap();
    runtime.load("commands", include_str!("../../../examples/plugins/commands.lua")).unwrap();
    let result = runtime.call_command("project", serde_json::json!({"session":"s", "workspace":"/project", "raw_input":" path"})).unwrap();
    assert_eq!(result.message, "/project");
    assert_eq!(runtime.command_metadata("project").1.len(), 2);
}

struct NativeText;

#[async_trait]
impl rness_kernel::presentation::TextProvider for NativeText {
    async fn text(&self) -> Option<String> { Some("shared contract".into()) }
}

#[tokio::test]
async fn native_and_lua_text_providers_share_the_contract() {
    use rness_kernel::presentation::TextProvider;
    let host = LuaHost::spawn().unwrap();
    let providers: Vec<&dyn TextProvider> = vec![&NativeText, &host];
    assert_eq!(providers[0].text().await.as_deref(), Some("shared contract"));
    assert_eq!(providers[1].text().await, None);
    host.load("status", "rness.ui.statusline(function() return 'shared contract' end)").await.unwrap();
    assert_eq!(providers[0].text().await, providers[1].text().await);
    let cell = rness_tui::modules::ext_statusline::StatusText::default();
    for provider in providers { cell.refresh(provider).await; }
}

#[tokio::test]
async fn coordinated_unload_holds_engine_reservation_through_ui_update() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(SessionStore::new(dir.path()), Arc::new(Silent),
        registry.clone(), TurnConfig::default(), Arc::new(EventBus::default())));
    let host = LuaHost::spawn().unwrap();
    host.install_session(sessions.clone(), Arc::new(rness_engine::subagent::SubagentRuntime::new(sessions.clone(), 3)),
        registry.clone(), Default::default(), tokio::runtime::Handle::current(), "test/model".into()).await.unwrap();
    host.load("owned", "rness.tool.register{name='owned', run=function() return 'yes' end}; rness.ui.app{name='panel', view=function() return {} end}").await.unwrap();
    let installed = rness_lua::api::tools::sync_lua_tools(&registry, &host, &[]).await;
    let guard = sessions.try_extension_maintenance().unwrap();
    assert!(host.unload_coordinated("owned", installed.clone(), |_| panic!("busy teardown")).await.is_err());
    assert!(registry.get("owned").is_some());
    assert_eq!(host.call_tool("owned", serde_json::json!({})).await.unwrap(), "yes");
    drop(guard);
    let s = sessions.clone();
    let r = registry.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    assert!(host.unload_coordinated("owned", installed, move |snapshot| {
        assert!(s.try_extension_maintenance().is_err());
        assert!(r.get("owned").is_none());
        assert!(snapshot.apps.is_empty());
        tx.send(()).unwrap();
    }).await.unwrap());
    rx.await.unwrap();
    assert!(sessions.try_extension_maintenance().is_ok());
    assert!(!host.unload_coordinated("owned", vec![], |_| panic!("already removed")).await.unwrap());
}

struct Silent;

#[async_trait]
impl Provider for Silent {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, _request: StepRequest<'_>, _cancel: &CancellationToken) -> StepOutcome {
        StepOutcome::Committed(AssistantMessage {
            model: "fake-1".into(),
            content: vec![],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
            chunks: vec![],
        })
    }
}

/// Boot a host the way the real binary does: engine mounted BEFORE the
/// plugins load, so every rness.* namespace the examples use exists.
async fn booted_host(dir: &std::path::Path) -> LuaHost {
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir),
        Arc::new(Silent),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let subagents = Arc::new(rness_engine::subagent::SubagentRuntime::new(
        Arc::clone(&sessions),
        3,
    ));
    let host = LuaHost::spawn().unwrap();
    host.install_session(
        sessions,
        subagents,
        Arc::new(ToolRegistry::default()),
        Default::default(),
        tokio::runtime::Handle::current(),
        "test/model".into(),
    )
    .await
    .unwrap();
    host
}

fn examples() -> Vec<rness_lua::loader::PluginSource> {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    let mut sources = Vec::new();
    let mut names: Vec<_> = std::fs::read_dir(format!("{root}/plugins"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".lua"))
        .collect();
    names.sort();
    for n in names {
        sources.push(rness_lua::loader::PluginSource {
            name: n.clone(),
            source: std::fs::read_to_string(format!("{root}/plugins/{n}")).unwrap(),
        });
    }
    sources
}

#[tokio::test(flavor = "multi_thread")]
async fn example_plugins_load_and_register_three_apps() {
    let dir = tempfile::tempdir().unwrap();
    let host = booted_host(dir.path()).await;
    let errors = rness_lua::loader::load_all(&host, &examples()).await;
    assert!(errors.is_empty(), "shipped plugins failed: {errors:?}");

    let apps = host.app_specs().await;
    let names: Vec<_> = apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, vec!["branches", "sessions", "tree"]);

    let tree = apps.iter().find(|a| a.name == "tree").unwrap();
    assert_eq!(tree.slot, "sidebar");
    assert_eq!(tree.keymap.as_deref(), Some("ctrl+e"));
    let sessions = apps.iter().find(|a| a.name == "sessions").unwrap();
    assert_eq!(sessions.slot, "overlay");
    assert_eq!(sessions.keymap.as_deref(), Some("ctrl+s"));
}

#[tokio::test(flavor = "multi_thread")]
async fn text_tool_and_bottomline_examples_work() {
    let dir = tempfile::tempdir().unwrap();
    let host = booted_host(dir.path()).await;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins");
    for name in ["text-tools", "bottomline", "session-log"] {
        host.load(name, &std::fs::read_to_string(root.join(format!("{name}.lua"))).unwrap()).await.unwrap();
    }
    assert_eq!(host.call_tool("text_stats", serde_json::json!({"text":"hello world\nnext"})).await.unwrap(), "bytes=16 words=3 lines=2");
    assert!(host.call_tool("text_stats", serde_json::json!({"text": 1})).await.is_err());
    assert_eq!(host.statusline().await.as_deref(), Some("rness | 0 running"));
    host.fire_hook("turn_start", serde_json::json!({"session":"example"}));
    assert_eq!(host.statusline().await.as_deref(), Some("rness | 1 running | last session example"));
    host.fire_hook("turn_end", serde_json::json!({"session":"example"}));
    assert_eq!(host.statusline().await.as_deref(), Some("rness | 0 running | last session example"));
}

#[tokio::test(flavor = "multi_thread")]
async fn keymap_binds_drain_in_declaration_order() {
    let dir = tempfile::tempdir().unwrap();
    let host = booted_host(dir.path()).await;
    let plugin = r#"
        rness.keymaps.set("ctrl+k", "scroll_up")
        rness.keymaps.set("ctrl+j", "scroll_down")
        rness.keymaps.set("ctrl+d", false)
    "#;
    host.load("keys.lua", plugin).await.unwrap();

    let binds = host.keymap_binds().await;
    assert_eq!(
        binds,
        vec![
            ("ctrl+k".to_string(), Some("scroll_up".to_string())),
            ("ctrl+j".to_string(), Some("scroll_down".to_string())),
            ("ctrl+d".to_string(), None),
        ]
    );

    // Validation is host-side: rebuild applies good binds, reports bad.
    let km = rness_tui::keymaps::KeymapState::stock();
    let errs = km.rebuild(&binds);
    assert!(errs.is_empty(), "{errs:?}");
    let entries: Vec<String> =
        km.entries().iter().map(|(_, a)| a.name().to_string()).collect();
    assert!(entries.contains(&"scroll_up".to_string()));
    // ctrl+d unbound: quit no longer present via that chord.
    let bad = km.rebuild(&[("hyper+q".into(), Some("scroll_up".into()))]);
    assert_eq!(bad.len(), 1, "unparseable chord must be reported");
}

#[tokio::test(flavor = "multi_thread")]
async fn tree_view_renders_and_navigates() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.path().join("README.md"), "# hi").unwrap();

    let host = booted_host(dir.path()).await;
    assert!(rness_lua::loader::load_all(&host, &examples()).await.is_empty());

    // Point the tree at the temp dir by cd'ing the VM's notion of cwd:
    // tree.lua uses ".", so run the process-wide chdir carefully — instead
    // just verify it renders the real cwd without error and navigation
    // round-trips.
    let ctx = serde_json::json!({"session": "s"});
    let lines = host.app_view("tree", ctx.clone()).await.unwrap();
    assert!(!lines.is_empty());
    assert!(lines[0].starts_with("> "), "cursor on first row: {:?}", &lines[0]);

    use rness_lua::runtime::AppKeyOutcome;
    assert_eq!(host.app_key("tree", "j", ctx.clone()).await.unwrap(), AppKeyOutcome::Consumed);
    let after = host.app_view("tree", ctx.clone()).await.unwrap();
    assert!(after.get(1).is_some_and(|l| l.starts_with("> ")), "cursor moved: {after:?}");

    // Unknown key passes through to the host keymap.
    assert_eq!(host.app_key("tree", "z", ctx).await.unwrap(), AppKeyOutcome::Pass);
}
