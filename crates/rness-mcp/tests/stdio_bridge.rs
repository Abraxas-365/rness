//! End-to-end against a REAL child process speaking MCP over stdio: a
//! tiny python server. Handshake, tools/list pagination, tools/call
//! (success + isError), and disconnect unregistering.

use std::sync::Arc;
use std::time::Duration;

use rness_engine::tools::ToolRegistry;
use rness_mcp::{McpConnection, StdioServer};

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
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        cursor = msg.get("params", {}).get("cursor")
        if cursor is None:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "greet", "description": "say hello", "inputSchema": {"type": "object", "properties": {"who": {"type": "string"}}}}], "nextCursor": "page2"}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "fail", "description": "always errors", "inputSchema": {"type": "object"}}]}})
    elif method == "tools/call":
        name = msg["params"]["name"]
        if name == "greet":
            who = msg["params"].get("arguments", {}).get("who", "?")
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "hola " + who}]}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "boom"}], "isError": True}})
    else:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "no such method"}})
"#;

fn fake_server(dir: &std::path::Path) -> StdioServer {
    let script = dir.join("server.py");
    std::fs::write(&script, FAKE_SERVER).unwrap();
    StdioServer {
        name: "fake".into(),
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        env: vec![],
        timeout: Duration::from_secs(5),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bridges_paginated_tools_and_calls_through() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let conn = McpConnection::connect(fake_server(dir.path())).await.unwrap();

    let names = conn.bridge_tools(&registry).await.unwrap();
    assert_eq!(names, vec!["mcp__fake__greet", "mcp__fake__fail"]);
    assert!(registry.names().contains(&"mcp__fake__greet".to_string()));

    // Success path through the registry (as the dispatcher would).
    let tool = registry.get("mcp__fake__greet").unwrap();
    let out = tool.execute(serde_json::json!({ "who": "rness" })).await.unwrap();
    assert_eq!(out, "hola rness");

    // isError path: an Err result, not a panic.
    let tool = registry.get("mcp__fake__fail").unwrap();
    let err = tool.execute(serde_json::json!({})).await.unwrap_err();
    assert_eq!(err, "boom");

    // Disconnect unregisters everything it registered.
    conn.disconnect(&registry).await;
    assert!(!registry.names().iter().any(|n| n.starts_with("mcp__fake__")));
}

#[tokio::test(flavor = "multi_thread")]
async fn spawn_failure_is_a_loud_typed_error() {
    let result = McpConnection::connect(StdioServer {
        name: "ghost".into(),
        command: "/nonexistent/binary".into(),
        args: vec![],
        env: vec![],
        timeout: Duration::from_secs(1),
    })
    .await;
    let err = match result {
        Ok(_) => panic!("spawn should fail"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("ghost"));
}
