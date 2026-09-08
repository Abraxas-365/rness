//! rness.mcp from Lua: connect a REAL stdio MCP server (python fake),
//! see its tools appear on the shared registry, call one through the
//! registry, disconnect and see them vanish. Connections are HOST-owned:
//! they survive VM hot reloads.

use std::sync::Arc;

use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::EventBus;
use rness_protocol::events::*;
use async_trait::async_trait;
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
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "pong!"}]}})
"#;

#[tokio::test(flavor = "multi_thread")]
async fn lua_connects_mcp_and_bridged_tools_survive_reload() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("server.py");
    std::fs::write(&script, FAKE_SERVER).unwrap();

    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(Silent),
        Arc::clone(&registry),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let subagents =
        Arc::new(rness_engine::subagent::SubagentRuntime::new(Arc::clone(&sessions), 3));

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
            local tools = rness.mcp.connect{{
                name = "fake",
                command = "python3",
                args = {{ "{}" }},
                timeout_ms = 5000,
            }}
            assert(tools[1] == "mcp__fake__ping", "bridged: " .. tostring(tools[1]))
            assert(rness.mcp.servers()[1] == "fake", "listed")
            "#,
            script.display()
        ),
    )
    .await
    .unwrap();

    // The bridged tool is callable through the shared registry.
    let tool = registry.get("mcp__fake__ping").unwrap();
    assert_eq!(tool.execute(serde_json::json!({})).await.unwrap(), "pong!");

    // Hot reload swaps the VM; the HOST owns the connection, so the
    // bridged tool keeps working and the fresh VM still sees the server.
    host.reload(vec![rness_lua::loader::PluginSource {
        name: "check.lua".into(),
        source: r#"assert(rness.mcp.servers()[1] == "fake", "connection survived reload")"#.into(),
    }])
    .await
    .unwrap();
    let tool = registry.get("mcp__fake__ping").unwrap();
    assert_eq!(tool.execute(serde_json::json!({})).await.unwrap(), "pong!");

    // Disconnect from Lua unregisters the bridged tools.
    host.load(
        "bye.lua",
        r#"assert(rness.mcp.disconnect("fake") == true)"#,
    )
    .await
    .unwrap();
    assert!(registry.get("mcp__fake__ping").is_none());
}
