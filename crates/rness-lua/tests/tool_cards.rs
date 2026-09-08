//! rness.ui.tool_card: Lua renders finished tool calls into styled
//! lines; nil / missing / throwing renderers fall back (None).

use rness_lua::plugin_host::LuaHost;
use rness_lua::runtime::StyledLine;

#[tokio::test(flavor = "multi_thread")]
async fn tool_card_renders_declines_and_survives_errors() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "cards.lua",
        r#"
        rness.ui.tool_card("edit", function(call)
            if call.is_error then return nil end -- decline
            return {
                { text = "file " .. call.args.file_path, style = "title" },
                "plain shorthand",
                { text = "+" .. call.args.new_string, style = "added" },
            }
        end)
        rness.ui.tool_card("boom", function() error("broken renderer") end)
        "#,
    )
    .await
    .unwrap();

    let args = serde_json::json!({ "file_path": "a.rs", "new_string": "x" });
    // Renders.
    let lines = host.tool_card("edit", args.clone(), "ok", false).await.unwrap();
    assert_eq!(
        lines,
        vec![
            StyledLine { text: "file a.rs".into(), style: "title".into() },
            StyledLine { text: "plain shorthand".into(), style: "".into() },
            StyledLine { text: "+x".into(), style: "added".into() },
        ]
    );
    // Declines on error result.
    assert!(host.tool_card("edit", args.clone(), "fail", true).await.is_none());
    // No renderer for this tool.
    assert!(host.tool_card("read", args.clone(), "ok", false).await.is_none());
    // Throwing renderer falls back instead of breaking.
    assert!(host.tool_card("boom", args, "ok", false).await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn catch_all_card_applies_to_any_tool() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "catchall.lua",
        r#"
        rness.ui.tool_card("*", function(call)
            return { { text = "tool: " .. call.name, style = "dim" } }
        end)
        "#,
    )
    .await
    .unwrap();
    let lines = host
        .tool_card("whatever", serde_json::json!({}), "out", false)
        .await
        .unwrap();
    assert_eq!(lines[0].text, "tool: whatever");
}
