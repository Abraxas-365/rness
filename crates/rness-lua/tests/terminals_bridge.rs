//! `rness.terminals` against a real registry, and the default terminals
//! plugin (monitor, tool card, statusline) on top of it.
#![cfg(unix)]

use rness_kernel::presentation::TextProvider;
use rness_lua::plugin_host::LuaHost;
use rness_tools::terminal::{ShellChoice, TerminalConfig, TerminalRegistry};
use serde_json::json;

const PLUGIN: &str = include_str!("../../../flavors/default/plugins/terminals.lua");

fn never() -> bool {
    false
}

#[tokio::test]
async fn terminals_api_reads_without_consuming_and_enforces_ownership() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "unmounted",
        r#"
        for _, name in ipairs({'count', 'list', 'inspect', 'stop', 'close'}) do
            local ok, err = pcall(rness.terminals[name], 'owner', 'term-1')
            assert(not ok and tostring(err):find('rness.terminals is not installed', 1, true))
        end
    "#,
    )
    .await
    .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let terminals = TerminalRegistry::new();
    let opened = terminals
        .open(
            Some("build".into()),
            ShellChoice::Controlled,
            dir.path().to_path_buf(),
            "owner".into(),
        )
        .unwrap();
    let id = opened.id.clone();
    let registry = terminals.clone();
    let sent_id = id.clone();
    tokio::task::spawn_blocking(move || {
        registry
            .send(
                &sent_id,
                "echo hello-terminals; false",
                true,
                Some(5000),
                "owner",
                &never,
            )
            .unwrap()
    })
    .await
    .unwrap();

    host.install_terminals(terminals.clone()).await.unwrap();
    // Sticky across hot reloads.
    host.reload(vec![]).await.unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    host.load(
        "check",
        &format!(
            r#"
        local c = rness.terminals.count('owner')
        assert(c.open == 1 and c.running == 0, 'count')
        assert(rness.terminals.count('foreign').open == 0)
        assert(#rness.terminals.list('foreign') == 0)
        for _, name in ipairs({{'inspect', 'stop', 'close'}}) do
            assert(not pcall(rness.terminals[name], 'foreign', {id:?}))
            assert(not pcall(rness.terminals[name], 'owner', 'term-99'))
        end
        local t = rness.terminals.list('owner')[1]
        assert(t.id == {id:?} and t.name == 'build', 'list')
        assert(t.running == false and t.state == 'idle at prompt', t.state)
        assert(t.command == 'echo hello-terminals; false', tostring(t.command))
        assert(t.last_exit == 1, 'last_exit ' .. tostring(t.last_exit))
        assert(t.uptime_secs >= 0 and t.command_secs >= 0)
        local i = rness.terminals.inspect('owner', {id:?}, 5)
        assert(i.output:find('hello-terminals', 1, true), i.output)
        assert(i.cwd == nil or i.cwd == {cwd:?}, 'cwd ' .. tostring(i.cwd))
        assert(rness.terminals.stop('owner', {id:?}) == false, 'nothing running')
    "#,
            cwd = cwd.to_string_lossy()
        ),
    )
    .await
    .unwrap();

    // Inspection did not consume: the model still reads the output.
    let (unread, _) = terminals.read(&id, None, "owner").unwrap();
    assert!(unread.contains("hello-terminals"), "{unread}");

    host.load(
        "close",
        &format!("assert(rness.terminals.close('owner', {id:?}) == true)"),
    )
    .await
    .unwrap();
    assert!(terminals.list("owner").is_empty());
}

#[tokio::test]
async fn stop_escalates_off_the_vm_thread() {
    let host = LuaHost::spawn().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let terminals = TerminalRegistry::new();
    let id = terminals
        .open(
            None,
            ShellChoice::Controlled,
            dir.path().to_path_buf(),
            "owner".into(),
        )
        .unwrap()
        .id;
    let registry = terminals.clone();
    let sent_id = id.clone();
    // Ignores INT and TERM: only the KILL step ends it.
    tokio::task::spawn_blocking(move || {
        registry
            .send(
                &sent_id,
                "bash -c 'trap \"\" INT TERM; while :; do sleep 0.1; done'",
                true,
                Some(500),
                "owner",
                &never,
            )
            .unwrap()
    })
    .await
    .unwrap();
    host.install_terminals(terminals.clone()).await.unwrap();
    let started = std::time::Instant::now();
    host.load(
        "stop",
        &format!(
            "assert(rness.terminals.count('owner').running == 1); \
             assert(rness.terminals.stop('owner', {id:?}) == true)"
        ),
    )
    .await
    .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "stop blocked the VM for {:?}",
        started.elapsed()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while terminals.running_count("owner") > 0 {
        assert!(std::time::Instant::now() < deadline, "stop never finished");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    terminals.close_all();
}

#[tokio::test]
async fn plugin_monitor_card_and_statusline() {
    use rness_lua::runtime::AppKeyOutcome::Consumed;
    let dir = tempfile::tempdir().unwrap();
    let terminals = TerminalRegistry::with_config(TerminalConfig::default());
    let id = terminals
        .open(
            Some("server".into()),
            ShellChoice::Controlled,
            dir.path().to_path_buf(),
            "one".into(),
        )
        .unwrap()
        .id;
    let registry = terminals.clone();
    let sent_id = id.clone();
    tokio::task::spawn_blocking(move || {
        registry
            .send(
                &sent_id,
                "echo monitor-line; sleep 30",
                true,
                Some(700),
                "one",
                &never,
            )
            .unwrap()
    })
    .await
    .unwrap();

    let host = LuaHost::spawn().unwrap();
    host.install_terminals(terminals.clone()).await.unwrap();
    host.load("terminals", PLUGIN).await.unwrap();

    let ctx = json!({"session":"one","rows":10,"cols":100});
    let lines = host.app_view("terminals", ctx.clone()).await.unwrap();
    assert!(lines[0].contains("(1)"), "{lines:?}");
    assert!(
        lines[1].starts_with("> term-1 server") && lines[1].contains("running"),
        "{lines:?}"
    );
    assert!(lines[1].contains("sleep 30"), "{lines:?}");
    assert_eq!(
        host.app_key("terminals", "enter", ctx.clone())
            .await
            .unwrap(),
        Consumed
    );
    let detail = host
        .app_view("terminals", ctx.clone())
        .await
        .unwrap()
        .join("\n");
    assert!(detail.contains("monitor-line"), "{detail}");
    assert!(detail.contains("FOLLOW"), "{detail}");
    // Other sessions see nothing.
    let other = host
        .app_view("terminals", json!({"session":"two","rows":10,"cols":100}))
        .await
        .unwrap();
    assert!(other[1].contains("No terminals"), "{other:?}");

    // `s` in the detail view stops the command without blocking.
    assert_eq!(
        host.app_key("terminals", "s", ctx.clone()).await.unwrap(),
        Consumed
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while terminals.running_count("one") > 0 {
        assert!(std::time::Instant::now() < deadline, "stop never finished");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Tool card from the send presentation.
    let card = host
        .tool_card_presented(
            "terminal_send",
            json!({"session_id": id, "text": "cargo test"}),
            "running 3 tests\ntest ok\n[exit code: 101]",
            false,
            Some(
                json!({"version":1, "kind":"terminal", "terminal":"term-1", "sent":"cargo test",
                "outcome":"exited", "exit_code":101, "elapsed_ms":2300}),
            ),
        )
        .await
        .unwrap();
    assert!(card[0].is_header);
    let left: String = card[0].spans.iter().map(|s| s.text.as_str()).collect();
    let right: String = card[0].right.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(left, "Terminal");
    assert_eq!(right, "exit 101 · 2.3s");
    assert_eq!(card[0].right[0].style, json!("error"));
    let body: Vec<String> = card
        .iter()
        .skip(1)
        .map(|l| {
            if l.spans.is_empty() {
                l.text.clone()
            } else {
                l.spans.iter().map(|s| s.text.as_str()).collect()
            }
        })
        .collect();
    assert_eq!(body, ["term-1 $ cargo test", "running 3 tests", "test ok"]);
    // Errors and presentation-less calls fall back to the built-in card.
    assert!(host
        .tool_card_presented("terminal_send", json!({}), "boom", true, None)
        .await
        .is_none());

    // `x` closes the selected terminal.
    host.app_key("terminals", "esc", ctx.clone()).await.unwrap();
    host.app_key("terminals", "x", ctx.clone()).await.unwrap();
    assert!(terminals.list("one").is_empty());
}

#[tokio::test]
async fn statusline_counts_terminals() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "mocks",
        r#"
        rness.session = {usage=function() return {input=0} end, config=function() return {} end}
        rness.terminals.count = function(session)
            if session == 'one' then return {open=2, running=1} end
            if session == 'two' then return {open=1, running=0} end
            return {open=0, running=0}
        end
    "#,
    )
    .await
    .unwrap();
    host.load(
        "statusline",
        include_str!("../../../flavors/default/plugins/statusline.lua"),
    )
    .await
    .unwrap();
    let text = host
        .status(json!({"session":"one","model":"test"}))
        .await
        .unwrap()
        .to_string();
    assert!(text.contains("2 terms (1 running)"), "{text}");
    let text = host
        .status(json!({"session":"two"}))
        .await
        .unwrap()
        .to_string();
    assert!(
        text.contains("1 term\"") && !text.contains("running"),
        "{text}"
    );
    let text = host
        .status(json!({"session":"three"}))
        .await
        .unwrap()
        .to_string();
    assert!(!text.contains("term"), "{text}");
}
