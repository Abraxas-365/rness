use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rness_mcp::{
    http::{HttpServer, SsePolicy},
    McpConnection,
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[derive(Default)]
struct Fixture {
    gets: Mutex<Vec<Option<String>>>,
    calls: AtomicUsize,
    lists: AtomicUsize,
    replies: AtomicUsize,
    deletes: AtomicUsize,
    call_id: Mutex<Value>,
    cancelled: Mutex<Option<Value>>,
    mode: u8,
}
fn auth(headers: &HeaderMap) {
    assert_eq!(headers["authorization"], "Bearer secret");
    assert_eq!(headers["mcp-session-id"], "session");
    assert_eq!(headers["mcp-protocol-version"], "2025-03-26");
}
fn sse(text: String) -> Response {
    ([("content-type", "text/event-stream")], text).into_response()
}
async fn post(
    State(state): State<Arc<Fixture>>,
    headers: HeaderMap,
    Json(msg): Json<Value>,
) -> Response {
    if msg["method"] != "initialize" {
        auth(&headers);
    }
    let result = match msg["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion":"2025-03-26", "capabilities":{"tools":{"listChanged":true}}})
        }
        Some("notifications/initialized") => return StatusCode::ACCEPTED.into_response(),
        Some("notifications/cancelled") => {
            *state.cancelled.lock().unwrap() = Some(msg["params"]["requestId"].clone());
            return StatusCode::ACCEPTED.into_response();
        }
        Some("tools/list") => {
            state.lists.fetch_add(1, Ordering::SeqCst);
            json!({"tools":[{"name":"wait", "inputSchema":{"type":"object"}}]})
        }
        Some("tools/call") => {
            state.calls.fetch_add(1, Ordering::SeqCst);
            *state.call_id.lock().unwrap() = msg["id"].clone();
            if state.mode == 3 {
                return sse(
                    "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n".into(),
                );
            }
            // Commit one event; interrupt a subsequent event mid-frame.
            return sse("id: call-1\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\nid: not-committed\ndata: {".into());
        }
        None => {
            state.replies.fetch_add(1, Ordering::SeqCst);
            return StatusCode::ACCEPTED.into_response();
        }
        _ => json!({}),
    };
    (
        [("mcp-session-id", "session")],
        Json(json!({"jsonrpc":"2.0","id":msg["id"],"result":result})),
    )
        .into_response()
}
async fn get(State(state): State<Arc<Fixture>>, headers: HeaderMap) -> Response {
    auth(&headers);
    assert_eq!(headers["accept"], "text/event-stream");
    let cursor = headers
        .get("last-event-id")
        .map(|s| s.to_str().unwrap().to_owned());
    state.gets.lock().unwrap().push(cursor.clone());
    if state.mode == 4 {
        return sse("id: call-1\n\n".into());
    }
    if state.mode == 5 {
        return sse("id: bounded\n\n".into());
    }
    if state.mode == 1 {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if state.mode == 2 {
        return StatusCode::NOT_FOUND.into_response();
    }
    if cursor.as_deref() == Some("call-1") {
        let id = state.call_id.lock().unwrap().clone();
        return sse(format!(
            "id: call-2\ndata: {}\n\n",
            json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}})
        ));
    }
    if cursor.is_none() {
        return sse("id: notice-1\rdata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\r\r".into());
    }
    assert_eq!(cursor.as_deref(), Some("notice-1"));
    // Repeated cursor must not dispatch its server ping twice. Hold the stream
    // open beyond the ordinary request timeout to test long-lived GET behavior.
    let frames = futures_util::stream::iter([Ok::<_, std::io::Error>(
        "id: notice-1\ndata: {\"jsonrpc\":\"2.0\",\"id\":90,\"method\":\"ping\"}\n\nid: notice-2\ndata: {\"jsonrpc\":\"2.0\",\"id\":91,\"method\":\"ping\"}\n\n"
    )]);
    use futures_util::StreamExt;
    let body = axum::body::Body::from_stream(frames.chain(futures_util::stream::pending()));
    ([("content-type", "text/event-stream")], body).into_response()
}
async fn fixture(
    mode: u8,
    notifications: bool,
    resume: bool,
) -> (
    Arc<Fixture>,
    Arc<McpConnection>,
    tokio::task::JoinHandle<()>,
) {
    let state = Arc::new(Fixture {
        mode,
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let router = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::post(post).get(get).delete(
                |State(state): State<Arc<Fixture>>| async move {
                    state.deletes.fetch_add(1, Ordering::SeqCst);
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let conn = McpConnection::connect_http(HttpServer {
        name: "stream".into(),
        url,
        headers: vec![("Authorization".into(), "Bearer secret".into())],
        timeout: Duration::from_millis(200),
        sse: SsePolicy {
            notifications,
            resume,
            max_attempts: 2,
            retry_delay_ms: 5,
            idle_timeout_ms: 5000,
        },
    })
    .await
    .unwrap();
    (state, conn, task)
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn post_stream_resumes_via_get_without_replaying_call() {
    let (state, conn, server) = fixture(0, false, true).await;
    let result = conn.request("tools/call", json!({})).await.unwrap();
    assert_eq!(result, json!({"ok":true}));
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    assert_eq!(*state.gets.lock().unwrap(), vec![Some("call-1".into())]);
    assert!(!conn.is_closed());
    conn.disconnect(&Default::default()).await;
    server.abort();
}

#[tokio::test]
async fn standalone_notifications_resume_refresh_catalog_and_stop_on_disconnect() {
    let (state, conn, server) = fixture(0, true, true).await;
    let registry = Arc::new(rness_engine::tools::ToolRegistry::default());
    conn.bridge_tools(&registry).await.unwrap();
    conn.watch(&registry, false).await;
    until(|| state.replies.load(Ordering::SeqCst) == 1 && state.lists.load(Ordering::SeqCst) >= 2)
        .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!conn.is_closed(), "GET must outlive normal POST timeout");
    assert_eq!(
        *state.gets.lock().unwrap(),
        vec![None, Some("notice-1".into())]
    );
    conn.disconnect(&registry).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(state.gets.lock().unwrap().len(), 2);
    server.abort();
}

#[tokio::test]
async fn optional_get_405_keeps_post_connection_healthy() {
    let (state, conn, server) = fixture(1, true, true).await;
    until(|| state.gets.lock().unwrap().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    conn.request("tools/list", json!({})).await.unwrap();
    assert!(!conn.is_closed());
    conn.disconnect(&Default::default()).await;
    server.abort();
}

#[tokio::test]
async fn expired_get_session_closes_without_reinitialization() {
    let (state, conn, server) = fixture(2, true, true).await;
    until(|| conn.is_closed()).await;
    assert_eq!(state.gets.lock().unwrap().len(), 1);
    assert_eq!(state.calls.load(Ordering::SeqCst), 0);
    conn.disconnect(&Default::default()).await;
    server.abort();
}

#[tokio::test]
async fn disconnect_cancels_inflight_post_recovery() {
    let (state, conn, server) = fixture(4, false, true).await;
    let pending = conn.clone();
    let task = tokio::spawn(async move { pending.request("tools/call", json!({})).await });
    until(|| state.calls.load(Ordering::SeqCst) == 1).await;
    conn.disconnect(&Default::default()).await;
    assert!(task.await.unwrap().is_err());
    let count = state.gets.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(state.gets.lock().unwrap().len(), count);
    server.abort();
}

#[tokio::test]
async fn tool_cancellation_notifies_server_without_call_replay() {
    let (state, conn, server) = fixture(4, false, true).await;
    let registry = rness_engine::tools::ToolRegistry::default();
    conn.bridge_tools(&registry).await.unwrap();
    let tool = registry.get("mcp__stream__wait").unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let token = cancel.clone();
    let task = tokio::spawn(async move {
        tool.execute_rich(&"session".into(), "call", json!({}), &token)
            .await
    });
    until(|| state.calls.load(Ordering::SeqCst) == 1).await;
    cancel.cancel();
    assert!(task
        .await
        .unwrap()
        .unwrap_err()
        .contains("remote termination is not confirmed"));
    assert_eq!(
        *state.cancelled.lock().unwrap(),
        Some(state.call_id.lock().unwrap().clone())
    );
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    conn.disconnect(&registry).await;
    server.abort();
}

#[tokio::test]
async fn standalone_retry_budget_is_bounded() {
    let (state, conn, server) = fixture(5, true, true).await;
    until(|| conn.is_closed()).await;
    assert_eq!(state.gets.lock().unwrap().len(), 3);
    conn.disconnect(&Default::default()).await;
    server.abort();
}

#[tokio::test]
async fn disabled_resume_or_missing_cursor_never_replays_post() {
    for (mode, resume) in [(0, false), (3, true)] {
        let (state, conn, server) = fixture(mode, false, resume).await;
        assert!(conn.request("tools/call", json!({})).await.is_err());
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        assert!(state.gets.lock().unwrap().is_empty());
        conn.disconnect(&Default::default()).await;
        server.abort();
    }
}
