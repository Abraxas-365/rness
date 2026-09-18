//! rness.ui.tool_card: Lua renders finished tool calls into styled
//! lines; nil / missing / throwing renderers fall back (None).

use rness_lua::plugin_host::LuaHost;
use rness_lua::runtime::StyledLine;

#[tokio::test(flavor = "multi_thread")]
async fn runaway_renderers_fall_back_and_release_vm_limits() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "bounded",
        r#"
        rness.ui.tool_card('loop', function() while true do end end)
        rness.ui.tool_card('allocate', function() return {string.rep('x', 32*1024*1024)} end)
        rness.ui.tool_card('healthy', function() return {'ok'} end)
    "#,
    )
    .await
    .unwrap();
    for name in ["loop", "allocate"] {
        assert!(host
            .tool_card(name, serde_json::json!({}), "", false)
            .await
            .is_none());
        assert_eq!(
            host.tool_card("healthy", serde_json::json!({}), "", false)
                .await
                .unwrap()[0]
                .text,
            "ok"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn presentation_reaches_diff_cards_and_old_results_remain_supported() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "snapshots",
        r#"
        rness.ui.messagebox.tool_card('Edit', function(call)
            if call.presentation == nil then return {call.output} end
            local p = call.presentation
            assert(p.version == 1)
            return {header={left={{text=p.path}}}, body={{
                kind='diff', before=p.before, after=p.after,
                old_start=p.old_start, new_start=p.new_start,
            }}}
        end)
    "#,
    )
    .await
    .unwrap();
    let metadata = serde_json::json!({"version":1,"path":"gone.rs", "before":"old\n", "after":"new\n", "old_start":7,"new_start":7});
    let rows = rness_engine::presentation::ToolCards::tool_card_presented(
        &host,
        "Edit",
        serde_json::json!({}),
        "unchanged model output",
        false,
        Some(metadata),
    )
    .await
    .unwrap();
    assert_eq!(rows[0].spans[0].text, "gone.rs");
    let block = rows[1].block.as_ref().unwrap();
    assert_eq!(block["before"], "old\n");
    assert_eq!(block["after"], "new\n");
    assert_eq!(block["old_start"], 7);
    let old = host
        .tool_card("Edit", serde_json::json!({}), "legacy", false)
        .await
        .unwrap();
    assert_eq!(old[0].text, "legacy");
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_structured_fields_fall_back() {
    let host = LuaHost::spawn().unwrap();
    host.load("malformed", r#"
        rness.ui.tool_card('badtext', function() return {{text=42}} end)
        rness.ui.tool_card('badspan', function() return {{spans='wrong'}} end)
        rness.ui.tool_card('baddiff', function() return {{kind='diff',before={},after='ok'}} end)
        rness.ui.tool_card('badnumber', function() return {{kind='code',text='x',start_line=-1}} end)
    "#).await.unwrap();
    for name in ["badtext", "badspan", "baddiff", "badnumber"] {
        assert!(
            host.tool_card(name, serde_json::json!({}), "", false)
                .await
                .is_none(),
            "{name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn headerless_structured_cards_split_multispan_lines() {
    let host = LuaHost::spawn().unwrap();
    host.load("multiline", r#"
        rness.ui.tool_card('test', function()
            return {body={{spans={{text='one\ntwo',style='added'},{text=' three\nfour',style='dim'}}}}}
        end)
    "#).await.unwrap();
    let rows = host
        .tool_card("test", serde_json::json!({}), "", false)
        .await
        .unwrap();
    let texts: Vec<String> = rows
        .iter()
        .map(|r| r.spans.iter().map(|s| s.text.as_str()).collect())
        .collect();
    assert_eq!(texts, ["one", "two three", "four"]);
    assert!(rows.iter().all(|r| !r.is_header));
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_and_recursive_cards_fall_back() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "bounds",
        r#"
        rness.ui.tool_card('large', function() return {string.rep('x', 262145)} end)
        rness.ui.tool_card('recursive', function() local t = {}; t[1] = t; return t end)
        rness.ui.tool_card('deep', function()
            local t = {}; local root = t
            for i=1,20 do t[1] = {}; t = t[1] end
            return root
        end)
    "#,
    )
    .await
    .unwrap();
    for name in ["large", "recursive", "deep"] {
        assert!(host
            .tool_card(name, serde_json::json!({}), "ok", false)
            .await
            .is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_cards_preserve_header_spans_and_direct_colors() {
    let host = LuaHost::spawn().unwrap();
    host.load("structured", r##"
        rness.ui.messagebox.tool_card('Bash', function(call)
            return {
                header={left={{text='Bash',style={fg='#fabd2f',bold=true}}, {text=' $ cmd',style='dim'}},
                        right={{text='done',style='added'}}},
                body={{text=call.output,style={bg='#282828'}}},
            }
        end)
    "##).await.unwrap();
    let rows = host
        .tool_card("Bash", serde_json::json!({}), "hello", false)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].is_header);
    assert!(!rows[1].is_header);
    assert_eq!(rows[0].spans[0].text, "Bash");
    assert_eq!(rows[0].spans[0].style["fg"], "#fabd2f");
    assert_eq!(rows[0].right[0].text, "done");
    assert_eq!(rows[1].spans[0].style["bg"], "#282828");
}

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
    let lines = host
        .tool_card("edit", args.clone(), "ok", false)
        .await
        .unwrap();
    assert_eq!(
        lines,
        vec![
            StyledLine {
                text: "file a.rs".into(),
                style: "title".into(),
                ..Default::default()
            },
            StyledLine {
                text: "plain shorthand".into(),
                style: "".into(),
                ..Default::default()
            },
            StyledLine {
                text: "+x".into(),
                style: "added".into(),
                ..Default::default()
            },
        ]
    );
    // Declines on error result.
    assert!(host
        .tool_card("edit", args.clone(), "fail", true)
        .await
        .is_none());
    // No renderer for this tool.
    assert!(host
        .tool_card("read", args.clone(), "ok", false)
        .await
        .is_none());
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
