//! rness.mcp from Lua: connect a REAL stdio MCP server (python fake),
//! see its tools appear on the shared registry, call one through the
//! registry, disconnect and see them vanish. Connections are HOST-owned:
//! they survive VM hot reloads.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::tools::{exposure::Exposure, ToolRegistry};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::EventBus;
use rness_protocol::events::*;
use tokio_util::sync::CancellationToken;

struct Silent;

#[async_trait]
impl Provider for Silent {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, _r: StepRequest<'_>, _c: &CancellationToken) -> StepOutcome {
        StepOutcome::Committed(AssistantMessage {
            model: "fake-1".into(),
            content: vec![],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
            estimated_input: 0,
            chunks: vec![],
        })
    }
}

const FAKE_SERVER: &str = r#"
import json, sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "0"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "ping", "description": "pong", "inputSchema": {"type": "object"}}]}})
    elif method == "tools/call":
        if msg.get('params', {}).get('arguments', {}).get('crash'): sys.exit(0)
        if len(sys.argv) > 1:
            with open(sys.argv[1], 'a') as calls: calls.write('called\n')
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "pong!"}]}})
"#;

#[tokio::test(flavor = "multi_thread")]
async fn lua_connects_mcp_and_bridged_tools_survive_reload() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("server.py");
    std::fs::write(&script, FAKE_SERVER).unwrap();

    let marker = dir.path().join("calls");
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(Silent),
        Arc::clone(&registry),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let subagents = Arc::new(rness_engine::subagent::SubagentRuntime::new(
        Arc::clone(&sessions),
        3,
    ));

    let host = rness_lua::plugin_host::LuaHost::spawn().unwrap();
    host.install_session(
        Arc::clone(&sessions),
        subagents,
        Arc::clone(&registry),
        Default::default(),
        tokio::runtime::Handle::current(),
        "test/model".into(),
    )
    .await
    .unwrap();

    host.load(
        "mcp-user.lua",
        &format!(
            r#"
            for _, sse in ipairs({{ {{max_attempts=101}}, {{retry_delay_ms=0}}, {{idle_timeout_ms=0}}, {{unknown=true}} }}) do
                local ok, failure = pcall(rness.mcp.connect, {{name='invalid', url='http://127.0.0.1:1/mcp', sse=sse}})
                assert(not ok and (tostring(failure):find('sse') or tostring(failure):find('unknown')), tostring(failure))
            end
            local tools = rness.mcp.connect{{
                name = "fake",
                command = "python3",
                args = {{ "{}", "{}" }},
                defer_tools = true,
                timeout_ms = 5000,
                reconnect = {{enabled=true, initial_delay_ms=10, max_delay_ms=50, max_attempts=2}},
            }}
            assert(tools[1] == "mcp__fake__ping", "bridged: " .. tostring(tools[1]))
            assert(rness.mcp.servers()[1] == "fake", "listed")
            "#,
            script.display(),
            marker.display()
        ),
    )
    .await
    .unwrap();

    assert!(registry.get("mcp__fake__ping").is_some());
    let exposure = Exposure::default();
    assert!(exposure
        .specs(&registry, &Default::default())
        .iter()
        .any(|tool| tool.name == "ToolSearch"));
    assert!(!exposure
        .specs(&registry, &Default::default())
        .iter()
        .any(|tool| tool.name == "mcp__fake__ping"));
    let (_, names) = exposure
        .search(
            &registry,
            &serde_json::json!({"query":"select:mcp__fake__ping"}),
        )
        .unwrap();
    assert!(exposure
        .specs(&registry, &names.into_iter().collect())
        .iter()
        .any(|tool| tool.name == "mcp__fake__ping"));

    let calls = vec![rness_engine::tools::ToolCall {
        call: "permission".into(),
        name: "mcp__fake__ping".into(),
        args: serde_json::json!({}),
    }];
    registry.approvals().set_rules(
        [(
            "mcp__fake__ping".into(),
            rness_engine::approval::ToolPolicy::Deny,
        )]
        .into(),
    );
    let result = registry
        .dispatch(&"s".into(), &calls, 1, &CancellationToken::new())
        .await;
    assert!(result[0].is_error);
    assert!(!marker.exists(), "denied MCP call reached the server");
    registry.approvals().set_rules(Default::default());
    let result = registry
        .dispatch(&"s".into(), &calls, 1, &CancellationToken::new())
        .await;
    assert!(!result[0].is_error);
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "called\n");

    // The bridged tool is callable through the shared registry.
    let tool = registry.get("mcp__fake__ping").unwrap();
    assert_eq!(tool.execute(serde_json::json!({})).await.unwrap(), "pong!");

    // Hot reload swaps the VM; the HOST owns the connection, so the
    // bridged tool keeps working and the fresh VM still sees the server.
    host.reload(vec![rness_lua::loader::PluginSource {
        dependencies: vec![],
        name: "check.lua".into(),
        source: r#"assert(rness.mcp.servers()[1] == "fake", "connection survived reload")"#.into(),
    }])
    .await
    .unwrap();
    let tool = registry.get("mcp__fake__ping").unwrap();
    assert_eq!(tool.execute(serde_json::json!({})).await.unwrap(), "pong!");

    host.load("nested.lua", &format!(r#"
        rness.mcp.connect{{name="fake__nested", command="python3", args={{"{}"}}, defer_tools=false}}
    "#, script.display())).await.unwrap();
    host.load(
        "reconnect.lua",
        r#"
        local names = rness.mcp.reconnect("fake")
        assert(#names == 1 and names[1] == "mcp__fake__ping")
    "#,
    )
    .await
    .unwrap();
    assert!(!registry.is_deferred("mcp__fake__nested__ping"));
    host.load("nested-bye.lua", r#"rness.mcp.disconnect("fake__nested")"#)
        .await
        .unwrap();
    assert!(registry.is_deferred("mcp__fake__ping"));
    assert_eq!(
        registry
            .get("mcp__fake__ping")
            .unwrap()
            .execute(serde_json::json!({}))
            .await
            .unwrap(),
        "pong!"
    );

    let previous = registry.get("mcp__fake__ping").unwrap();
    assert!(previous
        .execute(serde_json::json!({"crash":true}))
        .await
        .is_err());
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if registry
                .get("mcp__fake__ping")
                .is_some_and(|fresh| !Arc::ptr_eq(&fresh, &previous))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(registry.is_deferred("mcp__fake__ping"));
    assert_eq!(
        registry
            .get("mcp__fake__ping")
            .unwrap()
            .execute(serde_json::json!({}))
            .await
            .unwrap(),
        "pong!"
    );

    // Disconnect from Lua unregisters the bridged tools.
    host.load("bye.lua", r#"assert(rness.mcp.disconnect("fake") == true)"#)
        .await
        .unwrap();
    assert!(registry.get("mcp__fake__ping").is_none());
}

/// `background = true` returns before the handshake; tools appear later,
/// failures clean up the slot, and disconnect during a handshake is safe.
#[tokio::test(flavor = "multi_thread")]
async fn background_connect_returns_immediately_and_registers_later() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("server.py");
    // Slow handshake: the plugin load must not wait for it.
    let slow = FAKE_SERVER.replace(
        "import json, sys",
        "import json, sys, time\ntime.sleep(float(sys.argv[2]) if len(sys.argv) > 2 else 0)",
    );
    std::fs::write(&script, slow).unwrap();
    let marker = dir.path().join("calls");
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(Silent),
        Arc::clone(&registry),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let subagents = Arc::new(rness_engine::subagent::SubagentRuntime::new(
        Arc::clone(&sessions),
        3,
    ));
    let host = rness_lua::plugin_host::LuaHost::spawn().unwrap();
    host.install_session(
        Arc::clone(&sessions),
        subagents,
        Arc::clone(&registry),
        Default::default(),
        tokio::runtime::Handle::current(),
        "test/model".into(),
    )
    .await
    .unwrap();

    let started = std::time::Instant::now();
    host.load(
        "bg.lua",
        &format!(
            r#"
            local tools = rness.mcp.connect{{
                name = "bg", command = "python3", args = {{ "{script}", "{marker}", "0.8" }},
                defer_tools = true, timeout_ms = 10000, background = true,
            }}
            assert(#tools == 0, "background connect returns no tools yet")
            assert(rness.mcp.servers()[1] == "bg", "listed while connecting")
            local ok, failure = pcall(rness.mcp.connect, {{ name = "bg", command = "python3", background = true }})
            assert(not ok and tostring(failure):find("already connected"), tostring(failure))
            "#,
            script = script.display(),
            marker = marker.display(),
        ),
    )
    .await
    .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_millis(600),
        "plugin load waited for the handshake: {:?}",
        started.elapsed()
    );
    assert!(registry.get("mcp__bg__ping").is_none());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while registry.get("mcp__bg__ping").is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background tools registered");
    assert!(registry.is_deferred("mcp__bg__ping"));
    assert_eq!(
        registry
            .get("mcp__bg__ping")
            .unwrap()
            .execute(serde_json::json!({}))
            .await
            .unwrap(),
        "pong!"
    );
    host.load(
        "bg-reconnect.lua",
        r#"
        local names = rness.mcp.reconnect("bg")
        assert(#names == 1 and names[1] == "mcp__bg__ping")
        assert(rness.mcp.disconnect("bg") == true)
    "#,
    )
    .await
    .unwrap();
    assert!(registry.get("mcp__bg__ping").is_none());

    // A failing background connect is logged and frees the name.
    host.load(
        "bg-fail.lua",
        r#"rness.mcp.connect{ name = "broken", command = "/nonexistent/mcp-server", background = true }"#,
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let listed = host
                .load("bg-list.lua", r#"assert(#rness.mcp.servers() == 0)"#)
                .await;
            if listed.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("failed background connect removed from servers()");

    // Disconnect while the handshake is in flight: tools never appear.
    host.load(
        "bg-cancel.lua",
        &format!(
            r#"
            rness.mcp.connect{{ name = "gone", command = "python3", args = {{ "{script}", "{marker}", "0.5" }}, background = true }}
            assert(rness.mcp.disconnect("gone") == true)
            "#,
            script = script.display(),
            marker = marker.display(),
        ),
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(registry.get("mcp__gone__ping").is_none());
}
