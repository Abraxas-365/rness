
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
