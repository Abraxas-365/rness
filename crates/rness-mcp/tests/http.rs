use axum::{http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, Json};
use rness_mcp::{http::HttpServer, McpConnection};
use serde_json::{json, Value};
use std::time::Duration;

async fn handler(headers: HeaderMap, Json(message): Json<Value>) -> Response {
    if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some("Bearer test-secret") { return StatusCode::UNAUTHORIZED.into_response(); }
    if message["method"] != "initialize" {
        assert_eq!(headers["mcp-session-id"], "session-1");
        assert_eq!(headers["mcp-protocol-version"], "2025-03-26");
    }
    if message.get("method").is_none() { return StatusCode::ACCEPTED.into_response(); }
    let result = match message["method"].as_str().unwrap() {
        "rpc-error" => return Json(json!({"jsonrpc":"2.0", "id":message["id"], "error":{"code":-32602,"message":"bad arguments"}})).into_response(),
        "initialize" => json!({"protocolVersion":"2025-03-26", "capabilities":{"tools":{}}}),
        "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
        "tools/list" => json!({"tools":[{"name":"ping","inputSchema":{"type":"object"}}]}),
        "tools/call" => {
            let data = json!({"jsonrpc":"2.0", "id":message["id"], "result":{"content":[{"type":"text","text":"pong"}]}});
            let ping = json!({"jsonrpc":"2.0", "id":message["id"], "method":"ping"});
            return ([("content-type", "text/event-stream")], format!("data: {ping}\r\n\r\nevent: message\r\ndata: {data}\r\n\r\n")).into_response();
        },
        _ => json!({}),
    };
    ([("mcp-session-id", "session-1")], Json(json!({"jsonrpc":"2.0","id":message["id"],"result":result}))).into_response()
}

#[tokio::test]
async fn authenticated_json_and_sse_roundtrip() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().route("/mcp", axum::routing::post(handler).delete(|| async { StatusCode::NO_CONTENT }))).await.unwrap();
    });
    let conn = McpConnection::connect_http(HttpServer { name:"remote".into(), url, headers:vec![("Authorization".into(),"Bearer test-secret".into())], timeout:Duration::from_secs(2), sse:Default::default() }).await.unwrap();
    let registry = rness_engine::tools::ToolRegistry::default();
    assert!(matches!(conn.request("rpc-error", json!({})).await, Err(rness_mcp::McpError::Rpc { .. })));
    assert!(!conn.is_closed());
    conn.bridge_tools(&registry).await.unwrap();
    assert_eq!(registry.get("mcp__remote__ping").unwrap().execute(json!({})).await.unwrap(), "pong");
    conn.disconnect(&registry).await;
    assert!(registry.names().is_empty());
    server.abort();
}

#[tokio::test]
async fn remote_plaintext_and_reserved_headers_are_rejected() {
    for (url, headers) in [("http://example.com/mcp", vec![]), ("https://example.com/mcp", vec![("Host".into(), "evil".into())])] {
        assert!(McpConnection::connect_http(HttpServer { name:"remote".into(), url:url.into(), headers, timeout:Duration::from_millis(10), sse:Default::default() }).await.is_err());
    }
}
