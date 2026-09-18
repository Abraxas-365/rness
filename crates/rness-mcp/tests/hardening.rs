use rness_engine::tools::ToolRegistry;
use rness_mcp::{McpConnection, StdioServer};
use serde_json::json;
use std::{sync::Arc, time::Duration};

const SERVER: &str = r#"
import json, sys, os
version = 0
for line in sys.stdin:
    m = json.loads(line)
    method = m['method']
    if method == 'notifications/initialized': continue
    if method == 'hang': continue
    if method == 'crash': sys.exit(0)
    if method == 'huge':
        sys.stdout.write('x' * (17 * 1024 * 1024)); sys.stdout.flush(); continue
    result = {}
    if method == 'tools/list': result = {'tools':[{'name':'echo' + str(version), 'inputSchema':{'type':'object'}}]}
    if method == 'environment': result = dict(os.environ)
    if method == 'change':
        version += 1
        print(json.dumps({'jsonrpc':'2.0','method':'notifications/tools/list_changed'}), flush=True)
    print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':result}), flush=True)
    if method == 'stop-reading':
        import time
        time.sleep(30)
"#;
fn spec() -> StdioServer {
    StdioServer {
        name: "test".into(),
        command: "python3".into(),
        args: vec!["-u".into(), "-c".into(), SERVER.into()],
        env: vec![("EXPLICIT_KEY".into(), "chosen".into())],
        timeout: Duration::from_secs(2),
    }
}
#[tokio::test]
async fn environment_resync_eof_and_explicit_reconnect() {
    let registry = Arc::new(ToolRegistry::default());
    let conn = McpConnection::connect(spec()).await.unwrap();
    let env = conn.request("environment", json!({})).await.unwrap();
    assert_eq!(env["EXPLICIT_KEY"], "chosen");
    assert!(env.get("CARGO_MANIFEST_DIR").is_none());
    conn.bridge_tools(&registry).await.unwrap();
    conn.bridge_tools(&registry).await.unwrap(); // idempotent, not duplicate panic
    conn.watch(&registry, true).await;
    conn.request("change", json!({})).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while registry.get("mcp__test__echo1").is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(registry.get("mcp__test__echo0").is_none());
    assert!(registry.is_deferred("mcp__test__echo1"));
    assert!(conn.request("crash", json!({})).await.is_err());
    tokio::time::timeout(Duration::from_secs(3), async {
        while !registry.names().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let fresh = conn.reconnect(&registry).await.unwrap();
    assert!(registry.get("mcp__test__echo0").is_some());
    assert!(registry.is_deferred("mcp__test__echo0"));
    fresh.disconnect(&registry).await;
    assert!(!registry.has_deferred());
    let fresh = McpConnection::connect(spec()).await.unwrap();
    fresh.bridge_tools(&registry).await.unwrap();
    assert!(!registry.is_deferred("mcp__test__echo0"));
    fresh.disconnect(&registry).await;
}
#[tokio::test]
async fn concurrent_catalog_refresh_and_disconnect_clear_deferred_state() {
    let registry = Arc::new(ToolRegistry::default());
    let conn = McpConnection::connect(spec()).await.unwrap();
    conn.bridge_tools(&registry).await.unwrap();
    conn.watch(&registry, true).await;
    // Poll refresh first so it owns the catalog lock while disconnect queues.
    let (refresh, ()) =
        tokio::join!(biased; conn.bridge_tools(&registry), conn.disconnect(&registry));
    refresh.unwrap();
    assert!(registry.names().is_empty());
    assert!(!registry.has_deferred());
}

#[tokio::test]
async fn incomplete_writes_close_transport_on_timeout_or_cancellation() {
    for cancel in [false, true] {
        let mut server = spec();
        server.timeout = Duration::from_millis(500);
        let conn = McpConnection::connect(server).await.unwrap();
        conn.request("stop-reading", json!({})).await.unwrap();
        let writer = conn.clone();
        let task = tokio::spawn(async move {
            writer
                .request("blocked", json!({"data":"x".repeat(4 * 1024 * 1024)}))
                .await
        });
        if cancel {
            tokio::time::sleep(Duration::from_millis(200)).await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            assert!(task.await.unwrap().is_err());
        }
        let error = conn.request("environment", json!({})).await.unwrap_err();
        assert!(matches!(error, rness_mcp::McpError::Closed { .. }));
        conn.disconnect(&ToolRegistry::default()).await;
    }
}

#[tokio::test]
async fn oversized_frames_fail_without_waiting_for_newline() {
    let registry = ToolRegistry::default();
    let conn = McpConnection::connect(spec()).await.unwrap();
    assert!(conn.request("huge", json!({})).await.is_err());
    assert!(conn.request("environment", json!({})).await.is_err());
    conn.disconnect(&registry).await;
}
