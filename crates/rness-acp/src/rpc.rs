//! Minimal JSON-RPC 2.0 peer over newline-delimited JSON (ACP's stdio
//! framing). One writer channel keeps every outgoing line ordered; our own
//! requests to the client (`session/request_permission`) park on a oneshot
//! until the reader loop sees the matching response.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn invalid_params(message: impl std::fmt::Display) -> Self {
        Self::new(INVALID_PARAMS, format!("Invalid params: {message}"))
    }
    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new(INTERNAL_ERROR, message.to_string())
    }
    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("Method not found: {method}"))
    }
    fn to_json(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

type Waiter = oneshot::Sender<Result<Value, RpcError>>;

pub struct Peer {
    out: Mutex<Option<mpsc::UnboundedSender<String>>>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Waiter>>,
}

impl Peer {
    pub fn new(out: mpsc::UnboundedSender<String>) -> Self {
        Self {
            out: Mutex::new(Some(out)),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn send(&self, message: Value) {
        if let Some(out) = self.out.lock().unwrap().as_ref() {
            let _ = out.send(message.to_string());
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn respond(&self, id: Value, result: Result<Value, RpcError>) {
        self.send(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error.to_json() }),
        });
    }

    /// Send a request to the client and wait for its answer. Dropping the
    /// future (e.g. the turn was cancelled) forgets the pending entry; a
    /// late answer is then ignored.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let _guard = Forget { peer: self, id };
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        rx.await
            .unwrap_or_else(|_| Err(RpcError::internal("connection closed")))
    }

    /// Route a client response to the request that is waiting for it.
    pub fn resolve(&self, id: &Value, result: Result<Value, RpcError>) {
        let Some(id) = id.as_u64() else { return };
        if let Some(waiter) = self.pending.lock().unwrap().remove(&id) {
            let _ = waiter.send(result);
        }
    }

    /// Fail every outstanding request and close the output channel so the
    /// writer drains and exits.
    pub fn close(&self) {
        self.pending.lock().unwrap().clear();
        self.out.lock().unwrap().take();
    }
}

struct Forget<'a> {
    peer: &'a Peer,
    id: u64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.peer.pending.lock().unwrap().remove(&self.id);
    }
}

/// Parse a JSON-RPC error object received from the client.
pub fn error_from_json(value: &Value) -> RpcError {
    RpcError::new(
        value
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or(INTERNAL_ERROR),
        value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_string(),
    )
}
