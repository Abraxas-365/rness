
#[tokio::test]
async fn default_spinner_shows_session_compaction_and_restores_activity() {
    use rness_kernel::presentation::TextProvider;
    use serde_json::json;
    let host = rness_lua::plugin_host::LuaHost::spawn().unwrap();
    host.load("session-stub.lua", "rness.session = { usage = function() return { input = 0 } end, config = function() return {} end }").await.unwrap();
    host.load("spinner.lua", include_str!("../../../flavors/default/plugins/spinner.lua")).await.unwrap();
    let context = json!({"session":"s1","model":"test"});
    host.fire_hook("turn_start", json!({"session":"s1"}));
    host.fire_hook("frame", json!({"type":"delta","session":"s1","chunk":{"d":"thinking"}}));
    let before = host.status(context.clone()).await.unwrap();
    assert!(before.to_string().contains("thinking"));
    host.fire_hook("frame", json!({"type":"compaction_started","session":"s2"}));
    assert!(!host.status(context.clone()).await.unwrap().to_string().contains("compacting context"));
    host.fire_hook("frame", json!({"type":"compaction_started","session":"s1"}));
    assert!(host.status(context.clone()).await.unwrap().to_string().contains("compacting context"));
    host.fire_hook("frame", json!({"type":"compaction_finished","session":"s1","changed":false}));
    let after = host.status(context.clone()).await.unwrap().to_string();
    assert!(!after.contains("compacting context"));
    assert!(after.contains("thinking"));
    host.fire_hook("turn_end", json!({"session":"s1"}));
    host.fire_hook("frame", json!({"type":"compaction_started","session":"s1"}));
    assert!(host.status(context.clone()).await.unwrap().to_string().contains("compacting context"));
    host.fire_hook("frame", json!({"type":"compaction_finished","session":"s1","changed":true}));
    assert!(host.status(context.clone()).await.unwrap().to_string().contains("idle"));
    host.fire_hook("frame", json!({"type":"compaction_started","session":"s1"}));
    host.fire_hook("frame", json!({"type":"turn_idle","session":"s1"}));
    assert!(!host.status(context).await.unwrap().to_string().contains("compacting context"));
}

#[tokio::test(flavor = "multi_thread")]
async fn spinner_pattern_works() {
    let host = rness_lua::plugin_host::LuaHost::spawn().unwrap();
    host.load("probe.lua", r#"
        local running = {}
        rness.hook.on("turn_start", function(ev) running[ev.session] = true end)
        rness.hook.on("turn_end", function(ev) running[ev.session] = nil end)
        rness.ui.statusline(function()
            for _ in pairs(running) do return "BUSY" end
            return nil
        end)
    "#)
    .await
    .unwrap();
    assert_eq!(host.statusline().await, None, "idle: nil lets builtin render");
    host.fire_hook("turn_start", serde_json::json!({"session": "s1", "turn": 1}));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(host.statusline().await, Some("BUSY".into()), "busy: text");
    host.fire_hook("turn_end", serde_json::json!({"session": "s1", "turn": 1, "outcome": "Completed"}));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(host.statusline().await, None, "idle again");
}
