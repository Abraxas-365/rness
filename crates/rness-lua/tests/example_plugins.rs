//! Example plugins load through explicit file declarations after engine mount.
//! They are not embedded or auto-loaded; copying alone never activates them.

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
async fn structured_statusline_callbacks_replace_and_unload() {
    use rness_kernel::presentation::TextProvider;
    let host = LuaHost::spawn().unwrap();
    host.load("status", r##"
        rness.ui.statusline = {
            style = { fg = "#83a598" }, padding = { left = 1 },
            left = function(ctx) ctx.model = ctx.model .. "!"; return {{ text = ctx.model }} end,
            right = "static", visible = function(ctx) return ctx.busy end,
        }
    "##).await.unwrap();
    let context = serde_json::json!({"model":"test", "busy":true});
    let view = host.status(context.clone()).await.unwrap();
    assert_eq!(view["left"][0]["text"], "test!");
    assert_eq!(view["right"], "static");
    assert_eq!(view["visible"], true);
    assert_eq!(context["model"], "test");
    assert!(host.load("bad", "rness.ui.statusline = 42").await.is_err());
    host.reload(vec![]).await.unwrap();
    assert!(host.status(context).await.is_none());
}

#[tokio::test]
async fn default_subagent_renderer_bounds_activity_without_nested_dumps() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default/init.lua");
    let (host, _) = LuaHost::spawn_from_init(root).unwrap();
    let lines = host.tool_card_presented("subagent",
        serde_json::json!({"agent":"scout", "prompt":"Inspect 日本語\n".repeat(100)}), "RAW-CALL-OUTPUT", false,
        Some(serde_json::json!({"kind":"subagent_activity", "session":"01M2DTFV0CNHXA1QKQ02V5Q2J4", "status":"running",
            "activity":"RAW-ACTIVITY-ARGS", "elapsed_ms":2000,
            "lines":["> Bash RAW-LINES", "RAW-RESULT"], "streams":{"c":"RAW-STREAM"},
            "live":"Checking next file\n".repeat(100),
            "tools":[
                {"name":"Read", "status":"done", "args":{"path":"RAW-PATH"}, "output":"RAW-OUTPUT"},
                {"name":"Bash", "status":"running", "args":{"command":"RAW-COMMAND"}, "stream":"RAW-TOOL-STREAM"},
                {"name":"Grep", "status":"failed"}, {"name":"Bash", "status":"cancelled"}
            ]}))).await.unwrap();
    assert_eq!(lines.len(), 6);
    assert!(lines.iter().all(|line| line.structured && line.block.is_none()));
    assert!(lines[0].is_header);
    assert_eq!(lines[0].text, "scout · 02V5Q2J4 · running · 2s");
    assert!(lines[1].text.starts_with("Task: Inspect 日本語 Inspect"));
    assert!(lines[1].text.ends_with('…'));
    assert!(lines[1].text.chars().count() <= 246);
    assert_eq!(lines[2].text, "Activity: Bash");
    assert_eq!(lines[3].text, "Recent tools: 4 · 1 running · 1 done · 1 failed · 1 cancelled");
    assert_eq!(lines[3].style, "error");
    assert!(lines[4].text.ends_with('…'));
    assert!(lines[4].text.chars().count() <= 160);
    assert!(lines.iter().all(|line| !line.text.contains("RAW-")));
}

#[tokio::test]
async fn default_subagent_renderer_preserves_bounded_old_metadata_fallback() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default/init.lua");
    let (host, _) = LuaHost::spawn_from_init(root).unwrap();
    let lines = host.tool_card_presented("subagent", serde_json::json!({"agent":"scout", "prompt":"Inspect tests"}), "", false,
        Some(serde_json::json!({"kind":"subagent_activity", "status":"running", "activity":"Bash cargo test\n".repeat(100), "elapsed_ms":2000,
            "lines":["> Bash RAW-DUMP", "RAW-DUMP"], "streams":{"c":"RAW-DUMP"}, "live":"Checking next file"}))).await.unwrap();
    assert_eq!(lines[0].text, "scout · running · 2s");
    assert_eq!(lines[1].text, "Task: Inspect tests");
    assert!(lines[2].text.starts_with("Activity: Bash cargo test"));
    assert!(lines[2].text.ends_with('…'));
    assert!(lines[2].text.chars().count() <= 170);
    assert_eq!(lines[3].text, "Checking next file");
    assert_eq!(lines.len(), 5);
    assert!(lines.iter().all(|line| !line.text.contains("RAW-DUMP")));
}

#[tokio::test]
async fn default_subagent_renderer_handles_missing_metadata_and_errors() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default/init.lua");
    let (host, _) = LuaHost::spawn_from_init(root).unwrap();
    for meta in [None, Some(serde_json::json!("invalid")), Some(serde_json::json!({"kind":"other"}))] {
        assert!(host.tool_card_presented("subagent", serde_json::json!({}), "original failure", true, meta).await.is_none());
    }
    let lines = host.tool_card_presented("subagent", serde_json::Value::Null, "", false,
        Some(serde_json::json!({"kind":"subagent_activity"}))).await.unwrap();
    assert_eq!(lines[0].text, "subagent · unknown");
    assert_eq!(lines[1].text, "Task: Not available");
    assert_eq!(lines[2].text, "Activity: unknown");
    for status in ["completed", "cancelled", "error", "failed"] {
        let lines = host.tool_card_presented("subagent", serde_json::json!({}), "", false,
            Some(serde_json::json!({"kind":"subagent_activity", "status":status, "tools":[]}))).await.unwrap();
        assert_eq!(lines[0].text, format!("subagent · {}", if status == "completed" { "finished" } else { status }));
        assert_eq!(lines[0].style, if matches!(status, "error" | "failed") { "error" } else { "tool_name" });
        assert!(lines[3].text.starts_with("Recent tools: 0"));
    }
    let lines = host.tool_card_presented("subagent", serde_json::json!({"prompt":42}), &"failure\n".repeat(100), true,
        Some(serde_json::json!({"kind":"subagent_activity", "status":"running", "elapsed_ms":"invalid", "tools":[null, 42, {}], "live":false}))).await.unwrap();
    assert_eq!(lines[0].text, "subagent · failed");
    assert_eq!(lines[0].style, "error");
    assert_eq!(lines[lines.len() - 2].style, "error");
    assert!(lines[lines.len() - 2].text.chars().count() <= 200);
    assert_eq!(lines.last().unwrap().text, "Inspect: select card, i");
    for (args, meta) in [
        (serde_json::json!({"background_mode":"continuable"}), serde_json::json!({"kind":"subagent_activity", "status":"completed"})),
        (serde_json::json!({}), serde_json::json!({"kind":"subagent_activity", "status":"completed", "mode":"continuable"})),
    ] {
        let lines = host.tool_card_presented("subagent", args, "", false, Some(meta)).await.unwrap();
        assert_eq!(lines[0].text, "subagent · idle");
    }
}

#[tokio::test]
async fn default_send_message_renderer_preserves_actual_long_multiline_message() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default/init.lua");
    let (host, config) = LuaHost::spawn_from_init(root).unwrap();
    assert_eq!(config.messagebox["tools"]["send_message"]["display"], "preview");
    assert_eq!(config.messagebox["tools"]["send_message"]["preview_lines"], 4);
    for message in ["日本語 café ".repeat(1000), "First line\n\n  preserve indentation\n".repeat(30)] {
        for meta in [None, Some(serde_json::json!({"kind":"send_message", "agent_id":"stale-recipient", "accepted":true, "message":"not the actual message"}))] {
            let lines = host.tool_card_presented("send_message", serde_json::json!({"agent_id":"child-123", "message":message}),
                "message accepted by child-123", false, meta).await.unwrap();
            assert_eq!(lines[0].text, "Send message → child-123 · Accepted");
            assert!(lines[0].is_header);
            assert!(lines.iter().all(|line| line.structured));
            // The existing card preview/expand mechanism receives all message text.
            assert_eq!(lines[1..].iter().map(|line| line.text.as_str()).collect::<Vec<_>>().join("\n"), message);
        }
    }
}

#[tokio::test]
async fn default_send_message_renderer_handles_errors_and_metadata_fallback() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default/init.lua");
    let (host, _) = LuaHost::spawn_from_init(root).unwrap();
    for (is_error, meta) in [
        (true, None),
        (true, Some(serde_json::json!({"accepted":true}))),
        (false, Some(serde_json::json!({"accepted":false}))),
    ] {
        let lines = host.tool_card_presented("send_message", serde_json::json!({"agent_id":"child-123", "message":"Please review\nnext file"}),
            "message not delivered: unknown agent", is_error, meta).await.unwrap();
        assert_eq!(lines[0].text, "Send message → child-123 · failed");
        assert_eq!(lines[0].style, "error");
        assert_eq!(lines[1].text, "Please review");
        assert_eq!(lines[2].text, "next file");
        assert_eq!(lines[3].text, "message not delivered: unknown agent");
        assert_eq!(lines[3].style, "error");
    }
    let lines = host.tool_card_presented("send_message", serde_json::Value::Null, "", false,
        Some(serde_json::json!({"kind":"send_message", "agent_id":"legacy-child", "accepted":true}))).await.unwrap();
    assert_eq!(lines[0].text, "Send message → legacy-child · Accepted");
    assert_eq!(lines[1].text, "Message unavailable");
    let lines = host.tool_card_presented("send_message", serde_json::json!({"agent_id":42, "message":false}), "", true,
        Some(serde_json::json!("invalid"))).await.unwrap();
    assert_eq!(lines[0].text, "Send message → Unknown recipient · failed");
    assert_eq!(lines[1].text, "Message unavailable");
    assert_eq!(lines[2].text, "Message not accepted");
}

#[tokio::test]
async fn default_tool_renderers_respect_live_status() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default/init.lua");
    let (host, _) = LuaHost::spawn_from_init(root).unwrap();
    for (name, args) in [
        ("Bash", serde_json::json!({"command":"cargo test"})),
        ("Glob", serde_json::json!({"pattern":"*.rs"})),
        ("Grep", serde_json::json!({"pattern":"test"})),
        ("Read", serde_json::json!({"path":"src/lib.rs"})),
        ("Write", serde_json::json!({"path":"src/lib.rs", "content":"// test"})),
        ("send_message", serde_json::json!({"agent_id":"child", "message":"Review"})),
        ("web_search", serde_json::json!({"query":"Rust"})),
        ("web_fetch", serde_json::json!({"url":"https://example.com"})),
    ] {
        let lines = host.tool_card_presented(name, args, "", false,
            Some(serde_json::json!({"kind":"tool_live", "status":"running", "duration_ms":1200}))).await.unwrap();
        let header = &lines[0];
        assert!(header.is_header);
        assert!(header.text.contains("running") || header.spans.iter().chain(&header.right).any(|s| s.text == "running"), "{name}");
        assert!(!header.spans.iter().chain(&header.right).any(|s| s.text == "done"));
        if name == "Bash" {
            assert_eq!(header.right[0].text, "1200 ms");
            assert_eq!(lines[1].block.as_ref().unwrap()["text"], "cargo test");
        }
    }
    let lines = host.tool_card_presented("Bash", serde_json::json!({"command":"false"}), "failure", true,
        Some(serde_json::json!({"kind":"tool_live", "status":"running"}))).await.unwrap();
    assert!(lines[0].spans.iter().any(|span| span.text == "failed"));
}

#[tokio::test]
async fn default_task_renderer_shows_progress_and_states() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default/init.lua");
    let (host, _) = LuaHost::spawn_from_init(root).unwrap();
    host.load("tasks", include_str!("../../../flavors/default/plugins/tasks.lua")).await.unwrap();
    let tasks = serde_json::json!([
        {"content":"Done task", "status":"completed"},
        {"content":"Active task", "status":"in_progress"},
        {"content":"Pending task", "status":"pending"},
    ]);
    let lines = host.tool_card("TaskWrite", serde_json::json!({"tasks":tasks}), "updated", false).await.unwrap();
    assert!(lines[0].is_header);
    assert!(lines[0].spans.iter().any(|span| span.text == "Tasks"));
    assert!(lines[0].right.iter().any(|span| span.text == "1 active · 1 pending · 1 done"));
    assert!(lines.iter().any(|line| line.spans.iter().any(|span| span.text == "[=====-----------]")));
    assert!(lines.iter().any(|line| line.spans.iter().any(|span| span.text == "Active task")));
}

#[tokio::test]
async fn default_bash_and_write_renderers_show_command_and_content() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default/init.lua");
    let (host, _) = LuaHost::spawn_from_init(root).unwrap();

    let bash = host.tool_card_presented("Bash", serde_json::json!({"command":"printf 'hello'", "workdir":"/tmp"}), "hello", false,
        Some(serde_json::json!({"duration_ms":27}))).await.unwrap();
    assert!(bash[0].is_header);
    assert!(bash[0].spans.iter().any(|span| span.text == "Bash"));
    assert!(bash[0].spans.iter().any(|span| span.text == "done"));
    assert!(bash.iter().any(|line| line.block.as_ref().is_some_and(|block| block["text"] == "printf 'hello'")));
    assert!(bash.iter().any(|line| line.text == "hello"));

    let write = host.tool_card("Write", serde_json::json!({"path":"/tmp/test.lua", "content":"return true"}), "Wrote 11 bytes", false).await.unwrap();
    assert!(write[0].is_header);
    assert!(write[0].spans.iter().any(|span| span.text == "Write"));
    assert!(write[0].spans.iter().any(|span| span.text == "  /tmp/test.lua"));
    assert!(write.iter().any(|line| line.block.as_ref().is_some_and(|block| block["text"] == "return true")));
}

#[tokio::test]
async fn default_flavor_loads_with_explicit_plugins_and_small_scout() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default");
    let config = rness_lua::api::config::load(&root.join("init.lua")).unwrap();
    assert_eq!(config.colorscheme.as_deref(), Some("gruvbox"));
    assert_eq!(config.agents["scout"].profile.as_deref(), Some("small"));
    assert!(!config.allow_generic_subagents);
    assert!(std::fs::read_to_string(root.join("lua/agents.lua")).unwrap()
        .contains("rness.agents.allow_generic = false"));
    assert!(!config.agents.contains_key("worker"), "the optional worker role stays disabled by default");
    assert_eq!(config.messagebox["user"]["style"]["bg"], "#3c3836");
    assert_eq!(config.plugin_specs.len(), 12);
    let policy = &config.compaction["default"];
    assert_eq!(policy.threshold_tokens, 165000);
    assert_eq!(policy.prune_threshold, 8192);
    policy.validate().unwrap();
    let commands = std::fs::read_to_string(root.join("plugins/commands.lua")).unwrap();
    assert!(commands.contains("name = \"compact\""));
    assert!(!commands.contains("compact-region"));
    let specs = rness_lua::loader::discover_specs(&root, &config.plugin_specs).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()), Arc::new(Silent), registry.clone(),
        TurnConfig::default(), Arc::new(EventBus::default()),
    ));
    let host = LuaHost::spawn().unwrap();
    host.install_session(
        sessions.clone(),
        Arc::new(rness_engine::subagent::SubagentRuntime::new(sessions.clone(), 3)),
        registry, Default::default(), tokio::runtime::Handle::current(), "test/model".into(),
    ).await.unwrap();
    let questions = Arc::new(rness_engine::questions::Questions::default());
    host.install_questions(questions.clone()).await.unwrap();
    assert!(!questions.is_available());
    assert!(!sessions.reference_service().enabled());
    let errors = rness_lua::loader::load_all(&host, &specs).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert!(questions.is_available());
    let apps = host.app_specs().await;
    for name in ["branches", "sessions", "tasks"] {
        assert_eq!(apps.iter().find(|a| a.name == name).unwrap().refresh_ms, Some(500));
    }
    let plan = host.tool_specs().await.into_iter().find(|spec| spec.name == "exit_plan_mode").unwrap().plan.unwrap();
    assert!(Arc::ptr_eq(&plan.questions, &questions));
    assert!(sessions.reference_service().enabled());
    host.validate_bindings().await.unwrap();
    for (name, key, operation) in [
        ("delivery.queue", "<F8>", rness_lua::runtime::UiActionOperation::QueuePrompt),
        ("delivery.steer", "<F9>", rness_lua::runtime::UiActionOperation::SteerPrompt),
    ] {
        assert_eq!(host.call_action(name, "promptbox", serde_json::json!({})).await.unwrap(), vec![operation]);
        assert!(host.binding_specs().await.iter().any(|binding| binding.keys == vec![key]));
    }
}

#[tokio::test]
async fn explicit_file_lifecycle_remap_disable_reload_and_unload() {
    use rness_lua::loader::{PluginSpec, PluginLocation, discover_specs};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("review.lua");
    let source = "return function(opts, plugin) plugin.action('insert', {scope='promptbox', description='Insert', run=function(ctx) ctx.promptbox.insert(opts.text) end}); plugin.keys({insert={action='insert', key='<F6>'}}) end";
    std::fs::write(&path, source).unwrap();
    let mut spec = PluginSpec { dependencies: vec![], name: "review".into(), source: PluginLocation::File(path.clone()), enabled: true, watch: false, opts: serde_json::json!({"text":"first"}), keys: serde_json::json!({"insert":["<F8>","<F9>"]}) };
    let host = LuaHost::spawn().unwrap();
    let sources = discover_specs(root.path(), &[spec.clone()]).unwrap();
    assert!(rness_lua::loader::load_all(&host, &sources).await.is_empty());
    assert_eq!(host.binding_specs().await[0].keys, vec!["<F8>", "<F9>"]);
    assert_eq!(host.call_action("review.insert", "promptbox", serde_json::json!({})).await.unwrap(), vec![rness_lua::runtime::UiActionOperation::InsertPrompt("first".into())]);
    let generation = host.action_generation().load(std::sync::atomic::Ordering::SeqCst);
    std::fs::write(&path, "error('failed reload')").unwrap();
    assert!(host.reload(discover_specs(root.path(), &[spec.clone()]).unwrap()).await.is_err());
    assert_eq!(host.action_generation().load(std::sync::atomic::Ordering::SeqCst), generation);
    assert_eq!(host.binding_specs().await[0].keys.len(), 2);
    std::fs::write(&path, source).unwrap();
    spec.keys = serde_json::json!(false);
    host.reload(discover_specs(root.path(), &[spec.clone()]).unwrap()).await.unwrap();
    assert!(host.binding_specs().await.iter().all(|b| b.keys.is_empty()));
    assert!(host.unload("review").await.unwrap());
    assert!(host.action_specs().await.is_empty());
    host.reload(sources).await.unwrap();
    assert!(host.action_specs().await.is_empty());
    spec.enabled = false;
    spec.source = PluginLocation::File(root.path().join("missing.lua"));
    assert!(discover_specs(root.path(), &[spec.clone()]).unwrap().is_empty());
    spec.enabled = true;
    assert!(discover_specs(root.path(), &[spec]).is_err());
}

#[tokio::test]
async fn watcher_retries_busy_reload_without_another_save() {
    let root = tempfile::tempdir().unwrap();
    let host = LuaHost::spawn().unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(SessionStore::new(root.path().join("sessions")), Arc::new(Silent), registry.clone(), TurnConfig::default(), Arc::new(EventBus::default())));
    host.install_session(sessions.clone(), Arc::new(rness_engine::subagent::SubagentRuntime::new(sessions.clone(), 3)), registry.clone(), Default::default(), tokio::runtime::Handle::current(), "test/model".into()).await.unwrap();
    let path = root.path().join("live.plugin");
    std::fs::write(&path, "rness.tool.register{name='old', run=function() return 'old' end}").unwrap();
    let specs = vec![rness_lua::loader::PluginSpec {
        dependencies: vec![],
        name: "live".into(), source: rness_lua::loader::PluginLocation::File(path.clone()),
        enabled: true, watch: true, opts: serde_json::json!({}), keys: serde_json::json!({}),
    }];
    let sources = rness_lua::loader::discover_specs(root.path(), &specs).unwrap();
    assert!(rness_lua::loader::load_all(&host, &sources).await.is_empty());
    let installed = rness_lua::api::tools::sync_lua_tools(&registry, &host, &[]).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _watcher = rness_lua::reload::watch_specs(root.path().into(), specs, host.clone(), registry.clone(), installed, move |report| { let _ = tx.send(report); }).unwrap();
    let guard = sessions.try_extension_maintenance().unwrap();
    let replacement = root.path().join("replacement.tmp");
    std::fs::write(&replacement, "rness.tool.register{name='new', run=function() return 'new' end}").unwrap();
    std::fs::rename(replacement, path).unwrap();
    assert!(tokio::time::timeout(std::time::Duration::from_millis(700), rx.recv()).await.is_err());
    assert_eq!(host.call_tool("old", serde_json::json!({})).await.unwrap(), "old");
    drop(guard);
    let report = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    assert!(matches!(report, rness_lua::reload::ReloadReport::Reloaded { .. }), "{report:?}");
    assert!(registry.get("old").is_none());
    assert!(registry.get("new").is_some());
}

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

#[tokio::test]
async fn references_enable_disable_failed_reload_and_ownership() {
    let root = tempfile::tempdir().unwrap();
    let host = LuaHost::spawn().unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(SessionStore::new(root.path()), Arc::new(Silent), registry.clone(), TurnConfig::default(), Arc::new(EventBus::default())));
    host.install_session(sessions.clone(), Arc::new(rness_engine::subagent::SubagentRuntime::new(sessions.clone(), 3)), registry, Default::default(), tokio::runtime::Handle::current(), "test/model".into()).await.unwrap();
    host.load("references", "rness.file_references.enable { max_results = 3, respect_gitignore = false }").await.unwrap();
    assert!(sessions.reference_service().enabled());
    let generation = sessions.reference_service().generation();
    assert!(host.load("bad", "rness.file_references.disable(); error('rollback')").await.is_err());
    assert_eq!(sessions.reference_service().generation(), generation);
    assert!(sessions.reference_service().enabled());
    assert!(host.reload(vec![rness_lua::loader::PluginSource { dependencies: vec![], name: "broken".into(), source: "rness.file_references.disable(); error('broken')".into() }]).await.is_err());
    assert!(sessions.reference_service().enabled());
    host.load("replacement", "rness.file_references.enable()").await.unwrap();
    host.unload_coordinated("references", vec![], |_| {}).await.unwrap();
    assert!(sessions.reference_service().enabled());
    host.unload_coordinated("replacement", vec![], |_| {}).await.unwrap();
    assert!(!sessions.reference_service().enabled());
    host.load("on", "rness.file_references.enable()").await.unwrap();
    host.load("off", "rness.file_references.disable()").await.unwrap();
    assert!(!sessions.reference_service().enabled());
}

#[test]
fn command_example_registers_help_and_arguments() {
    let mut runtime = rness_lua::runtime::LuaRuntime::new().unwrap();
    runtime.load("commands", include_str!("../../../examples/plugins/commands.lua")).unwrap();
    let result = runtime.call_command("project", serde_json::json!({"session":"s", "workspace":"/project", "raw_input":" path"})).unwrap();
    assert_eq!(result.message, "/project");
    assert_eq!(runtime.command_metadata("project").1.len(), 2);
}

#[tokio::test]
async fn lua_permission_rules_survive_workspace_and_ceiling_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("executed");
    let init = dir.path().join("init.lua");
    std::fs::write(&init, "rness.permissions.set { guarded = 'deny' }").unwrap();
    let config = rness_lua::api::config::load(&init).unwrap();
    let host = LuaHost::spawn().unwrap();
    host.load("guarded", &format!("rness.tool.register{{name='guarded', run=function() rness.fs.write({:?}, 'ran'); return 'ok' end}}", marker.to_str().unwrap())).await.unwrap();
    let registry = ToolRegistry::default();
    rness_lua::api::tools::sync_lua_tools(&registry, &host, &[]).await;
    registry.approvals().set_rules(config.permissions);
    let calls = vec![rness_engine::tools::ToolCall { call: "c".into(), name: "guarded".into(), args: serde_json::json!({}) }];
    for session in ["parent", "child"] {
        let scoped = registry.for_workspace(&session.into(), dir.path()).restricted(&["guarded".into()]);
        let result = scoped.dispatch(&session.into(), &calls, 1, &CancellationToken::new()).await;
        assert!(result[0].is_error);
        assert!(!marker.exists());
    }
    registry.approvals().set_rules(Default::default());
    let result = registry.dispatch(&"parent".into(), &calls, 1, &CancellationToken::new()).await;
    assert!(!result[0].is_error);
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "ran");
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
async fn runtime_reload_preserves_init_and_rolls_back_commands_and_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let init = dir.path().join("init.lua");
    std::fs::write(&init, "boots = (boots or 0) + 1; rness.ui.statusline(function() return tostring(boots) end)").unwrap();
    let (host, _) = LuaHost::spawn_from_init(init).unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(SessionStore::new(dir.path().join("sessions")), Arc::new(Silent), registry.clone(), TurnConfig::default(), Arc::new(EventBus::default())));
    host.install_session(sessions.clone(), Arc::new(rness_engine::subagent::SubagentRuntime::new(sessions.clone(), 3)), registry, Default::default(), tokio::runtime::Handle::current(), "test/model".into()).await.unwrap();
    let source = |version: &str| rness_lua::loader::PluginSource { dependencies: vec![], name: "live".into(), source: format!("rness.commands.register{{name='live', description='{version}', arguments={{'{version}'}}, run=function() return '{version}' end}}; rness.hook.on('tick', function() ticks=(ticks or 0)+1 end)") };
    host.load("live", &source("v1").source).await.unwrap();
    let widgets = rness_lua::loader::PluginSource { dependencies: vec![], name: "widgets".into(), source: "rness.tool.register{name='widget', run=function() return 'new' end}; rness.ui.app{name='widget', view=function() return {'new'} end}; rness.ui.tool_card('widget', function() return {'new'} end)".into() };
    host.load("widgets", &widgets.source).await.unwrap();
    let guard = sessions.try_extension_maintenance().unwrap();
    assert!(host.reload(vec![source("v2")]).await.is_err());
    drop(guard);
    host.reload(vec![source("v2"), widgets]).await.unwrap();
    assert_eq!(host.call_tool("widget", serde_json::json!({})).await.unwrap(), "new");
    assert_eq!(host.app_view("widget", serde_json::json!({})).await.unwrap(), vec!["new"]);
    assert!(sessions.commands().completions().iter().any(|(name, _)| name == "live v2"));
    assert!(!sessions.commands().completions().iter().any(|(name, _)| name == "live v1"));
    let mut broken = source("broken"); broken.source.push_str("; error('broken')");
    assert!(host.reload(vec![broken]).await.is_err());
    assert!(sessions.commands().completions().iter().any(|(name, _)| name == "live v2"));
    host.fire_hook("tick", serde_json::json!({}));
    host.load("check", "assert(boots == 1); assert(ticks == 1)").await.unwrap();
    assert_eq!(host.statusline().await.as_deref(), Some("1"));
    host.reload(vec![]).await.unwrap();
    assert!(sessions.commands().resolve("/live").is_none());
    assert_eq!(host.statusline().await.as_deref(), Some("1"));
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

#[tokio::test(flavor = "multi_thread")]
async fn agents_command_lists_completes_and_stops_while_parent_runs() {
    use rness_engine::inbox::{Disposition, Phase};
    use rness_protocol::branch::{Delegation, DelegationMode};
    struct Waiting;
    #[async_trait]
    impl Provider for Waiting {
        fn model(&self) -> &str { "waiting" }
        async fn step(&self, _: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
            cancel.cancelled().await;
            StepOutcome::Cancelled { partial: vec![] }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(SessionStore::new(dir.path()), Arc::new(Waiting),
        registry.clone(), TurnConfig::default(), Arc::new(EventBus::default())));
    let runtime = Arc::new(rness_engine::subagent::SubagentRuntime::new(sessions.clone(), 3));
    let host = LuaHost::spawn().unwrap();
    host.install_session(sessions.clone(), runtime.clone(), registry, Default::default(),
        tokio::runtime::Handle::current(), "waiting".into()).await.unwrap();
    host.load("controls", include_str!("../../../flavors/default/plugins/agent-controls.lua")).await.unwrap();
    host.load("ordinary", "rness.commands.register{name='ordinary',run=function() return 'ok' end}").await.unwrap();
    let parent = sessions.create(None).unwrap();
    let unrelated = sessions.create(None).unwrap();
    let child = sessions.create_delegated(None, Delegation { parent: parent.clone(), depth: 1,
        call: None, mode: DelegationMode::Continuable }).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let grandchild = sessions.create_delegated(None, Delegation { parent: child.clone(), depth: 2,
        call: None, mode: DelegationMode::OneShot }).unwrap();
    for id in [&parent, &child, &grandchild, &unrelated] {
        sessions.send(id, UserIntent::Followup, vec![ContentPart::Text { text: "wait".into() }]).unwrap();
    }
    assert!(sessions.prepare_command(&parent, "/ordinary").is_err());
    assert_eq!(runtime.list_children(&parent, true).unwrap().len(), 1);
    assert_eq!(runtime.list_agents(&parent).unwrap().len(), 2);
    let prepared = sessions.prepare_command(&parent, "/agents stop ").unwrap().unwrap();
    let service = sessions.clone();
    let choices = tokio::task::spawn_blocking(move || prepared.complete(&service)).await.unwrap().unwrap();
    assert!(choices.contains(&"stop a1".into()));
    assert!(choices.contains(&"stop a2".into()));
    assert!(!choices.contains(&format!("stop {unrelated}")));
    async fn command(sessions: &Arc<SessionService>, parent: &str, text: String) -> Result<Disposition, rness_engine::service::ServiceError> {
        // Child settlement briefly reserves the parent operation lock. Wait for
        // that independent notification to retire, not for the parent turn.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let result = sessions.send_async(parent.into(), UserIntent::Followup,
                    vec![ContentPart::Text { text: text.clone() }]).await;
                if !matches!(result, Err(rness_engine::service::ServiceError::Busy)) { break result; }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.expect("command admission")
    }
    let result = command(&sessions, &parent, "/agents".into()).await.unwrap();
    assert!(matches!(result, Disposition::Command(result)
        if result.data == serde_json::json!({"action":"agents:open", "session":parent})));
    let result = command(&sessions, &parent, "/agents stop".into()).await.unwrap();
    assert!(matches!(result, Disposition::Command(result) if result.message.contains("a1  depth=1") && result.message.contains("a2  depth=2")));
    for id in [&parent, &unrelated, &"unknown".to_string()] {
        assert!(command(&sessions, &parent, format!("/agents stop {id}")).await.is_err());
    }
    assert!(command(&sessions, &parent, "/agents stop extra args".into()).await.is_err());
    assert!(command(&sessions, &parent, "/agents a2 steer".into()).await.is_err());
    assert!(command(&sessions, &parent, "/agents a2 steer redirect work".into()).await.is_err());
    let result = command(&sessions, &parent, "/agents a1 steer redirect work".into()).await.unwrap();
    assert!(matches!(result, Disposition::Command(result) if result.message.contains("User steering sent")));
    let result = command(&sessions, &parent, "/agents a2".into()).await.unwrap();
    assert!(matches!(result, Disposition::Command(result)
        if result.data == serde_json::json!({"action":"agents:open", "session":parent, "agent":grandchild})));
    command(&sessions, &parent, format!("/agents stop {child}")).await.unwrap();
    assert_eq!(sessions.phase(&child), Phase::Running);
    command(&sessions, &parent, format!("/agents {child} stop confirm")).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), sessions.join(&child)).await.unwrap();
    assert_eq!(sessions.phase(&parent), Phase::Running);
    assert_eq!(sessions.phase(&grandchild), Phase::Running);
    // Only one running descendant remains, so omitting the ID selects it for confirmation.
    let result = command(&sessions, &parent, "/agents stop".into()).await.unwrap();
    assert!(matches!(result, Disposition::Command(result)
        if result.message.contains(&format!("/agents {grandchild} stop confirm"))));
    assert_eq!(sessions.phase(&grandchild), Phase::Running);
    command(&sessions, &parent, format!("/agents {grandchild} stop confirm")).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), sessions.join(&grandchild)).await.unwrap();
    let result = command(&sessions, &parent, "/agents stop".into()).await.unwrap();
    assert!(matches!(result, Disposition::Command(result) if result.message == "No running subagents."));
    let prepared = sessions.prepare_command(&parent, "/agents stop ").unwrap().unwrap();
    let service = sessions.clone();
    let choices = tokio::task::spawn_blocking(move || prepared.complete(&service)).await.unwrap().unwrap();
    assert!(choices.contains(&"a1".into()));
    assert!(!choices.contains(&"stop a1".into()));
    assert!(!choices.contains(&"stop a2".into()));
    for id in [&parent, &unrelated] { sessions.cancel(id); sessions.join(id).await; }
    assert!(sessions.prepare_command(&parent, "/ordinary").unwrap().is_some());
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
    host.install_questions(Arc::new(rness_engine::questions::Questions::default())).await.unwrap();
    host
}

#[tokio::test(flavor = "multi_thread")]
async fn region_confirmation_reuses_only_its_own_command_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let session = log.session().clone();
    log.append(&SessionEvent::UserMessage(UserMessage { intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: "source ".repeat(100) }], source: None })).unwrap();
    drop(log);
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(store, Arc::new(Silent), registry.clone(), TurnConfig::default(), Arc::new(EventBus::default())));
    let host = LuaHost::spawn().unwrap();
    host.install_session(sessions.clone(), Arc::new(rness_engine::subagent::SubagentRuntime::new(sessions.clone(), 3)), registry, Default::default(), tokio::runtime::Handle::current(), "test/model".into()).await.unwrap();
    host.load("region", r#"
        rness.commands.register{name='region', run=function(ctx)
            local view = rness.session.compaction_view(ctx.session)
            local changed = rness.session.compact_region(ctx.session, {
                start=1, ['end']=1, sources=view.sources,
                policy={system_prompt='Summarize.',prompt='Preserve context.',threshold_tokens=24000,retain_tokens=4000,summary_tokens=200,
                    max_overflow_retries=1,max_compactions=2,prune_threshold=8192,prune_head=4096,prune_tail=1024}
            })
            return {message=changed and 'changed' or 'unchanged'}
        end}
    "#).await.unwrap();
    let prepared = sessions.prepare_command(&session, "/region").unwrap().unwrap();
    assert!(sessions.prepare_command(&session, "/region").is_err());
    assert!(sessions.try_extension_maintenance().is_err());
    let policy: rness_engine::turn::compaction::Policy = serde_json::from_value(serde_json::json!({
        "system_prompt":"Summarize.","prompt":"Preserve context.",
        "threshold_tokens":24000,"retain_tokens":4000,"summary_tokens":200,"max_overflow_retries":1,
        "max_compactions":2,"prune_threshold":8192,"prune_head":4096,"prune_tail":1024
    })).unwrap();
    let sources = sessions.replay(&session).unwrap().context.sources;
    assert!(sessions.compact_region(&session, 0, 1, sources, policy).await.is_err());
    let service = sessions.clone();
    let outcome = tokio::task::spawn_blocking(move || prepared.execute(&service)).await.unwrap().unwrap();
    assert!(matches!(outcome, rness_engine::inbox::Disposition::Command(result) if result.message == "unchanged"));
    assert!(!sessions.command_running(&session));
    assert!(sessions.try_extension_maintenance().is_ok());
    assert!(sessions.store().history(&session).unwrap().iter().any(|e| matches!(e.event, SessionEvent::CompactionStarted { .. })));
}

#[tokio::test]
async fn usage_reports_latest_request_and_durable_turn_count() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let session = log.session().clone();
    for turn in 1..=3 {
        log.append(&SessionEvent::TurnStarted { turn }).unwrap();
        log.append(&SessionEvent::AssistantMessage(AssistantMessage {
            model: "fake-1".into(), content: vec![], stop: StopReason::EndTurn,
            usage: Usage { input_tokens: u64::from(turn) * 100, output_tokens: 10, ..Default::default() },
            chunks: vec![],
        })).unwrap();
        log.append(&SessionEvent::TurnEnded { turn, outcome: TurnOutcome::Completed }).unwrap();
    }
    drop(log);
    let host = booted_host(dir.path()).await;
    host.load("usage-check", &format!(
        "local usage = rness.session.usage({session:?}); assert(usage.input == 300); assert(usage.output == 10); assert(usage.turns == 3)"
    )).await.unwrap();
}

#[tokio::test]
async fn plan_review_lua_configuration_is_validated_and_normalized() {
    let dir = tempfile::tempdir().unwrap();
    let host = booted_host(dir.path()).await;
    host.install_questions(Arc::new(rness_engine::questions::Questions::default())).await.unwrap();
    for options in ["{width=20}", "{height=2}", "{unknown=true}", "{editor={}}", "{keys={edit='a'}}", "{keys={edit='ctrl+c'}}", "{keys={typo='x'}}"] {
        assert!(host.load("bad-plan", &format!("rness.plan.enable {{ review = {options} }}")).await.is_err(), "{options}");
        assert!(!host.tool_specs().await.iter().any(|s| s.name == "exit_plan_mode"));
    }
    host.load("plan", r##"rness.plan.enable { review = {
        title = "Review changes", width = 88, height = 24,
        editor = { "code", "--wait" }, approve_after_edit = false,
        keys = { edit = "ctrl+e" }, labels = { approve = "Go" },
        styles = { border = { fg = "#aabbcc" } },
    } }"##).await.unwrap();
    let plan = host.tool_specs().await.into_iter().find(|s| s.name == "exit_plan_mode").unwrap().plan.unwrap();
    assert_eq!(plan.config.review.title, "Review changes");
    assert_eq!(plan.config.review.width, 88);
    assert_eq!(plan.config.review.keys["edit"], "ctrl+e");
    assert_eq!(plan.config.review.keys["approve"], "a");
    assert_eq!(plan.config.review.labels.approve, "Go");
    assert!(!plan.config.review.approve_after_edit);
}

#[tokio::test]
async fn plan_lifecycle_uses_live_questions_and_preserves_state() {
    let dir = tempfile::tempdir().unwrap();
    let host = booted_host(dir.path()).await;
    let questions = Arc::new(rness_engine::questions::Questions::default());
    questions.set_available(true);
    host.install_questions(questions.clone()).await.unwrap();
    host.load("plan", "rness.plan.enable()").await.unwrap();
    let spec = host.tool_specs().await.into_iter().find(|s| s.name == "exit_plan_mode").unwrap();
    let original = spec.plan.unwrap();
    assert!(Arc::ptr_eq(&original.questions, &questions));
    assert!(host.load("broken", "rness.plan.disable(); error('fail')").await.is_err());
    assert!(!original.alive.is_cancelled());
    host.load("off", "rness.plan.disable()").await.unwrap();
    assert!(original.alive.is_cancelled());
    assert!(!host.tool_specs().await.iter().any(|s| s.name == "exit_plan_mode"));
    host.load("replacement", "rness.plan.enable()").await.unwrap();
    host.unload_coordinated("plan", vec![], |_| {}).await.unwrap();
    assert!(host.tool_specs().await.iter().any(|s| s.name == "exit_plan_mode"));
    let old = host.tool_specs().await.into_iter().find(|s| s.name == "exit_plan_mode").unwrap().plan.unwrap();
    host.reload(vec![rness_lua::loader::PluginSource { dependencies: vec![], name: "fresh".into(), source: "rness.plan.enable()".into() }]).await.unwrap();
    assert!(old.alive.is_cancelled());
    let fresh = host.tool_specs().await.into_iter().find(|s| s.name == "exit_plan_mode").unwrap().plan.unwrap();
    assert!(Arc::ptr_eq(&fresh.questions, &questions));
    assert!(!fresh.alive.is_cancelled());
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
    let specs: Vec<_> = names.into_iter().map(|n| rness_lua::loader::PluginSpec {
        dependencies: if n == "plan.lua" { vec!["questions".into()] } else { vec![] },
        name: n.trim_end_matches(".lua").to_string(),
        source: rness_lua::loader::PluginLocation::File(std::path::Path::new(root).join("plugins").join(n)),
        enabled: true,
        watch: false,
        opts: serde_json::json!({}),
        keys: serde_json::json!({}),
    }).collect();
    sources.extend(rness_lua::loader::discover_specs(std::path::Path::new(root), &specs).unwrap());
    sources
}

#[test]
fn specialist_roles_load_explicitly_with_bounded_tool_permissions() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("lua")).unwrap();
    std::fs::write(root.path().join("lua/roles.lua"), include_str!("../../../examples/lua/roles.lua")).unwrap();
    let init = root.path().join("init.lua");
    std::fs::write(&init, "require('roles')").unwrap();
    let config = rness_lua::api::config::load(&init).unwrap();
    assert_eq!(config.agents.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["context-builder", "delegate", "oracle", "planner", "researcher", "reviewer", "scout", "worker"]);
    for (name, agent) in &config.agents {
        assert!(agent.subagent);
        assert!(agent.profile.is_none(), "examples inherit settings without invented models");
        assert!(!agent.instructions.is_empty());
        let tools = agent.tools.as_ref().unwrap();
        assert!(!tools.iter().any(|t| t == "subagent" || t == "*"));
        if !matches!(name.as_str(), "worker" | "delegate") {
            assert_eq!(tools, &["Glob", "Grep", "Read"]);
        } else {
            assert_eq!(tools, &["Glob", "Grep", "Read", "Edit", "Write", "Bash"]);
        }
    }
}

#[test]
fn example_init_loads_without_implicitly_enabling_plugins() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/init.lua");
    let config = rness_lua::api::config::load(&path).unwrap();
    assert!(config.plugin_specs.is_empty());
    assert!(config.plugins.is_empty());
    assert!(config.mappings.is_empty());
}

#[tokio::test]
async fn keymaps_example_supports_options_remapping_disable_and_unload() {
    let root = tempfile::tempdir().unwrap();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins/keymaps.lua");
    for (keys, expected) in [
        (serde_json::json!({}), vec!["<F6>"]),
        (serde_json::json!({"insert_review": ["<F8>", "<F9>"]}), vec!["<F8>", "<F9>"]),
        (serde_json::json!({"insert_review": false}), vec![]),
        (serde_json::json!(false), vec![]),
    ] {
        let init = root.path().join("init.lua");
        std::fs::write(&init, format!(
            "rness.plugins.setup({{{{name='keymaps', file={:?}, opts={{text='Custom review'}}, keys=rness.json.decode({:?})}}}})",
            path.to_str().unwrap(), keys.to_string(),
        )).unwrap();
        let (host, config) = LuaHost::spawn_from_init(init).unwrap();
        let sources = rness_lua::loader::discover_specs(root.path(), &config.plugin_specs).unwrap();
        assert!(rness_lua::loader::load_all(&host, &sources).await.is_empty());
        host.validate_bindings().await.unwrap();
        let bindings = host.binding_specs().await;
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].keys, expected);
        assert_eq!(bindings[0].action, "keymaps.insert_review");
        assert_eq!(host.call_action("keymaps.insert_review", "promptbox", serde_json::json!({})).await.unwrap(),
            vec![rness_lua::runtime::UiActionOperation::InsertPrompt("Custom review".into())]);
        assert!(host.unload("keymaps").await.unwrap());
        assert!(host.binding_specs().await.is_empty());
        assert!(host.action_specs().await.is_empty());
    }
}

#[tokio::test]
async fn task_wrapping_preserves_long_tokens_and_unicode() {
    let host = LuaHost::spawn().unwrap();
    host.load("fixture", "rness.session = { tasks = function() return {tasks={{id='x', content=string.rep('abcdefgh', 20)..' 日本語 café END-MARKER', status='completed'}}} end }").await.unwrap();
    host.load("tasks", include_str!("../../../examples/plugins/tasks.lua")).await.unwrap();
    let ctx = serde_json::json!({"session":"a", "rows":6, "cols":20});
    let first = host.app_view("tasks", ctx.clone()).await.unwrap();
    assert!(first[1].starts_with("[x]"));
    assert!(first[2].starts_with("    "));
    host.app_key("tasks", "end", ctx.clone()).await.unwrap();
    let last = host.app_view("tasks", ctx.clone()).await.unwrap();
    let content: String = last.iter().skip(1).map(|line| line.trim()).collect();
    assert!(content.contains("END-MARKER"), "{last:?}");
    host.app_key("tasks", "home", ctx.clone()).await.unwrap();
    assert_eq!(host.app_view("tasks", ctx).await.unwrap(), first);
}

#[tokio::test]
async fn task_overlay_scrolls_and_isolates_sessions() {
    let host = LuaHost::spawn().unwrap();
    host.load("fixture", "rness.session = { tasks = function(id) local tasks = {}; if id == 'a' then for i=1,20 do tasks[i] = {id=tostring(i),content='Task '..i,status='pending'} end end; return {tasks=tasks} end }").await.unwrap();
    host.load("tasks", include_str!("../../../examples/plugins/tasks.lua")).await.unwrap();
    let ctx = serde_json::json!({"session":"a", "rows":4});
    let lines = host.app_view("tasks", ctx.clone()).await.unwrap();
    assert_eq!(lines.len(), 21); // desired height is independent of clipped viewport
    assert!(lines[4..].iter().all(String::is_empty));
    assert!(lines[1].contains("Task 1"));
    host.app_key("tasks", "j", ctx.clone()).await.unwrap();
    assert!(host.app_view("tasks", ctx.clone()).await.unwrap()[1].contains("Task 2"));
    assert_eq!(host.app_view("tasks", serde_json::json!({"session":"b","rows":4})).await.unwrap()[1], "No saved tasks.");
    assert!(host.app_view("tasks", ctx).await.unwrap()[1].contains("Task 2"));
    host.unload("tasks").await.unwrap();
    assert!(host.app_specs().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn example_plugins_load_and_register_five_apps() {
    let dir = tempfile::tempdir().unwrap();
    let host = booted_host(dir.path()).await;
    let errors = rness_lua::loader::load_all(&host, &examples()).await;
    assert!(errors.is_empty(), "shipped plugins failed: {errors:?}");

    let apps = host.app_specs().await;
    let names: Vec<_> = apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, vec!["branches", "compact-region", "sessions", "tasks", "tree"]);
    for name in ["branches", "sessions", "tasks"] {
        assert_eq!(apps.iter().find(|a| a.name == name).unwrap().refresh_ms, Some(500));
    }
    assert_eq!(apps.iter().find(|a| a.name == "tree").unwrap().refresh_ms, None);
    let region = apps.iter().find(|a| a.name == "compact-region").unwrap();
    assert_eq!(region.key_help, vec!["j/k: move", "space: anchor", "enter: review", "r: refresh", "esc: close"]);

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
