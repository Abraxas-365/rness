//! MCP client bridge (dsh mcp-client, collapsed to our scale).
//!
//! One [`McpConnection`] speaks JSON-RPC 2.0 to ONE MCP server over
//! stdio (spawned child process). Its tools register on the engine's
//! ToolRegistry under server-qualified public names
//! (`mcp__<server>__<raw>`) — the raw name is only ever sent on the
//! wire, never recovered by parsing the public name.
//!
//! Scope (dsh parity): tools only. Resources and prompts are NOT
//! bridged. Nothing connects unless the host composes it — no config
//! magic, no auto-discovery.

pub mod http;
pub mod reconnect;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rness_engine::tools::{Tool, ToolRegistry};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::oneshot;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("mcp server '{server}' spawn failed: {source}")]
    Spawn {
        server: String,
        #[source]
        source: std::io::Error,
    },
    #[error("mcp server '{server}' invalid name (must be [A-Za-z0-9_-]{{1,32}})")]
    BadServerName { server: String },
    #[error("mcp '{server}': {message}")]
    Protocol { server: String, message: String },
    #[error("mcp '{server}': request timed out after {timeout_ms}ms")]
    Timeout { server: String, timeout_ms: u64 },
    #[error("mcp '{server}': remote RPC error: {message}")]
    Rpc { server: String, message: String },
    #[error("mcp '{server}': connection closed")]
    Closed { server: String },
}

/// How to reach one MCP server: a child process speaking stdio.
#[derive(Debug, Clone)]
pub struct StdioServer {
    /// Stable local namespace for public tool names. `[A-Za-z0-9_-]{1,32}`.
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Per-request timeout. No default — the host decides (0 magic).
    pub timeout: Duration,
}

/// Public, model-facing name for an MCP tool. Deterministic pure
/// function of (server, raw). Invalid chars become `_`; overlong names
/// are truncated with a stable hash suffix so identities never collide.
pub fn public_tool_name(server: &str, raw: &str) -> String {
    const MAX: usize = 64;
    let joined = format!("mcp__{server}__{raw}");
    let normalized: String = joined
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if normalized == joined && normalized.len() <= MAX {
        return normalized;
    }
    // Stable identity hash (FNV-1a over server\0raw) — no crypto needed,
    // just collision resistance across a tool list.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in server.as_bytes().iter().chain([0u8].iter()).chain(raw.as_bytes()) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let hash = format!("{h:016x}");
    let keep = MAX - hash.len() - 1;
    format!("{}_{hash}", &normalized[..keep.min(normalized.len())])
}

fn valid_server_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

struct PendingRequest { pending: Pending, id: u64 }
impl Drop for PendingRequest {
    fn drop(&mut self) { self.pending.lock().expect("pending lock").remove(&self.id); }
}

struct IncompleteWrite<'a> { connection: &'a McpConnection, complete: bool }
impl Drop for IncompleteWrite<'_> {
    fn drop(&mut self) {
        if self.complete { return; }
        let conn = self.connection;
        conn.closed.store(true, Ordering::Release);
        if let Some(reader) = &conn.reader { reader.abort(); }
        conn.pending.lock().expect("pending lock").clear();
        if let Some(child) = conn.child.lock().expect("child lock").as_mut() { let _ = child.start_kill(); }
        conn.changed.notify_one();
    }
}

/// A small launch-environment allowlist; credentials must be passed explicitly.
fn child_env() -> Vec<(String, std::ffi::OsString)> {
    ["PATH", "HOME", "USERPROFILE", "SYSTEMROOT", "WINDIR", "PATHEXT", "TEMP", "TMP", "TMPDIR", "LANG", "LC_ALL"]
        .into_iter().filter_map(|key| std::env::var_os(key).map(|value| (key.into(), value))).collect()
}

async fn read_frame(reader: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> std::io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return if frame.is_empty() { Ok(None) } else { Err(std::io::ErrorKind::UnexpectedEof.into()) };
        }
        let end = bytes.iter().position(|b| *b == b'\n').map(|i| i + 1);
        let count = end.unwrap_or(bytes.len());
        if frame.len().saturating_add(count) > MAX_FRAME_BYTES {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "MCP frame exceeds 16 MiB"));
        }
        frame.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if end.is_some() { return Ok(Some(frame)); }
    }
}

/// A live connection to one MCP server. Dropping it kills the child;
/// `disconnect` additionally unregisters the bridged tools.
pub struct McpConnection {
    server: String,
    spec: Option<StdioServer>,
    http: Option<Arc<http::HttpTransport>>,
    stdin: Option<Arc<tokio::sync::Mutex<ChildStdin>>>,
    pending: Pending,
    next_id: AtomicU64,
    timeout: Duration,
    child: Mutex<Option<Child>>,
    /// Public names registered on behalf of this connection.
    registered: Mutex<Vec<std::sync::Weak<dyn Tool>>>,
    closed: Arc<AtomicBool>,
    reader: Option<tokio::task::AbortHandle>,
    http_reader: Mutex<Option<tokio::task::AbortHandle>>,
    cancel: tokio_util::sync::CancellationToken,
    sync: tokio::sync::Mutex<()>,
    changed: Arc<tokio::sync::Notify>,
    deferred: AtomicBool,
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(reader) = &self.reader { reader.abort(); }
        if let Some(reader) = self.http_reader.get_mut().unwrap().take() { reader.abort(); }
    }
}

impl McpConnection {
    /// Spawn the server, run the MCP initialize handshake, and return
    /// the live connection. Does NOT register tools yet — see
    /// [`Self::bridge_tools`].
    pub async fn connect(spec: StdioServer) -> Result<Arc<Self>, McpError> {
        if !valid_server_name(&spec.name) {
            return Err(McpError::BadServerName { server: spec.name });
        }
        let mut cmd = tokio::process::Command::new(&spec.command);
        cmd.args(&spec.args)
            .env_clear()
            .envs(child_env())
            .envs(spec.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|source| McpError::Spawn { server: spec.name.clone(), source })?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let changed = Arc::new(tokio::sync::Notify::new());
        let reader_closed = closed.clone();
        let reader_changed = changed.clone();
        let route = Arc::clone(&pending);
        let server_name = spec.name.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout);
            while let Ok(Some(line)) = read_frame(&mut lines).await {
                let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
                    tracing::warn!(server = %server_name, "mcp: invalid JSON; closing connection");
                    break;
                };
                if msg["method"] == "notifications/tools/list_changed" {
                    reader_changed.notify_one();
                    continue;
                }
                let Some(id) = msg["id"].as_u64() else { continue };
                let Some(tx) = route.lock().expect("pending lock").remove(&id) else {
                    continue;
                };
                let result = if msg["error"].is_object() {
                    Err(msg["error"]["message"].as_str().unwrap_or("unknown error").to_string())
                } else {
                    Ok(msg["result"].clone())
                };
                let _ = tx.send(result);
            }
            reader_closed.store(true, Ordering::Release);
            reader_changed.notify_one();
            // EOF/protocol failure: fail calls, never replay side effects.
            for (_, tx) in route.lock().expect("pending lock").drain() {
                let _ = tx.send(Err("connection closed".into()));
            }
        });

        let conn = Arc::new(Self {
            spec: Some(spec.clone()),
            http: None,
            server: spec.name,
            stdin: Some(Arc::new(tokio::sync::Mutex::new(stdin))),
            pending,
            next_id: AtomicU64::new(1),
            timeout: spec.timeout,
            child: Mutex::new(Some(child)),
            registered: Mutex::new(Vec::new()),
            closed,
            changed,
            deferred: AtomicBool::new(false),
            reader: Some(reader.abort_handle()),
            http_reader: Default::default(), cancel: Default::default(),
            sync: tokio::sync::Mutex::new(()),
        });

        // MCP handshake: initialize → initialized notification.
        conn.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "rness", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await?;
        conn.notify("notifications/initialized", json!({})).await?;
        Ok(conn)
    }

    pub async fn connect_http(spec: http::HttpServer) -> Result<Arc<Self>, McpError> {
        if !valid_server_name(&spec.name) { return Err(McpError::BadServerName { server: spec.name }); }
        let transport = http::HttpTransport::new(spec.clone())?;
        let conn = Arc::new(Self {
            server: spec.name, spec: None, http: Some(transport.clone()), stdin: None,
            pending: Default::default(), next_id: AtomicU64::new(1), timeout: spec.timeout,
            child: Mutex::new(None), registered: Default::default(), closed: Arc::new(AtomicBool::new(false)),
            reader: None, http_reader: Default::default(), cancel: Default::default(), sync: Default::default(), changed: Default::default(), deferred: AtomicBool::new(false),
        });
        let result = match conn.request("initialize", json!({"protocolVersion":"2025-03-26", "capabilities":{}, "clientInfo":{"name":"rness", "version":env!("CARGO_PKG_VERSION")}})).await {
            Ok(result) => result,
            Err(error) => { transport.close().await; return Err(error); },
        };
        if let Err(error) = transport.set_version(result["protocolVersion"].as_str().unwrap_or("")) {
            transport.close().await;
            return Err(error);
        }
        if let Err(error) = conn.notify("notifications/initialized", json!({})).await {
            transport.close().await;
            return Err(error);
        }
        if transport.spec.sse.notifications {
            let changed = conn.changed.clone();
            let closed = conn.closed.clone();
            let reader = tokio::spawn(async move {
                if let Err(error) = transport.notifications(&changed).await {
                    tracing::warn!(%error, "MCP notification stream stopped");
                    closed.store(true, Ordering::Release);
                    changed.notify_one();
                }
            });
            *conn.http_reader.lock().unwrap() = Some(reader.abort_handle());
        }
        Ok(conn)
    }

    /// Explicit reconnection, never a replay of failed tool calls.
    pub async fn reconnect(&self, registry: &ToolRegistry) -> Result<Arc<Self>, McpError> {
        self.disconnect(registry).await;
        let conn = match &self.spec {
            Some(spec) => Self::connect(spec.clone()).await?,
            None => Self::connect_http(self.http.as_ref().expect("HTTP transport").spec.clone()).await?,
        };
        conn.deferred.store(self.deferred.load(Ordering::Acquire), Ordering::Release);
        if let Err(error) = conn.bridge_tools(registry).await {
            conn.disconnect(registry).await;
            return Err(error);
        }
        Ok(conn)
    }

    pub fn is_closed(&self) -> bool { self.closed.load(Ordering::Acquire) }

    pub fn server(&self) -> &str {
        &self.server
    }

    async fn send_raw(&self, payload: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(payload).expect("serializable");
        line.push('\n');
        if line.len() > MAX_FRAME_BYTES {
            return Err(McpError::Protocol { server: self.server.clone(), message: "outgoing MCP frame exceeds 16 MiB".into() });
        }
        let mut stdin = self.stdin.as_ref().expect("stdio transport").lock().await;
        if self.closed.load(Ordering::Acquire) { return Err(McpError::Closed { server: self.server.clone() }); }
        let mut guard = IncompleteWrite { connection: self, complete: false };
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|_| McpError::Closed { server: self.server.clone() })?;
        stdin.flush().await.map_err(|_| McpError::Closed { server: self.server.clone() })?;
        guard.complete = true;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        if let Some(http) = &self.http {
            let payload = json!({"jsonrpc":"2.0", "method":method, "params":params});
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(McpError::Closed { server: self.server.clone() }),
                result = http.post(&payload, &self.changed) => { result?; },
            }
            return Ok(());
        }
        self.send_raw(&json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    /// One JSON-RPC request with the connection's timeout.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(McpError::Closed { server: self.server.clone() });
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.request_with_id(id, method, params).await
    }

    async fn request_with_id(&self, id: u64, method: &str, params: Value) -> Result<Value, McpError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(McpError::Closed { server: self.server.clone() });
        }
        if let Some(http) = &self.http {
            let payload = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
            let work = http.post(&payload, &self.changed);
            let result = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(McpError::Closed { server: self.server.clone() }),
                result = tokio::time::timeout(self.timeout, work) => result
                    .unwrap_or_else(|_| Err(McpError::Timeout { server:self.server.clone(), timeout_ms:self.timeout.as_millis() as u64 })),
            };
            if result.as_ref().is_err_and(|error| !matches!(error, McpError::Rpc { .. })) { self.closed.store(true, Ordering::Release); self.changed.notify_one(); }
            return result?.ok_or_else(|| McpError::Protocol { server:self.server.clone(), message:"missing HTTP response".into() });
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        let _cleanup = PendingRequest { pending: self.pending.clone(), id };
        let timeout_ms = self.timeout.as_millis() as u64;
        let work = async {
            self.send_raw(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await?;
            match rx.await {
                Err(_) => Err(McpError::Closed { server: self.server.clone() }),
                Ok(Err(message)) => Err(McpError::Protocol { server: self.server.clone(), message }),
                Ok(Ok(result)) => Ok(result),
            }
        };
        tokio::time::timeout(self.timeout, work).await
            .unwrap_or_else(|_| Err(McpError::Timeout { server: self.server.clone(), timeout_ms }))
    }

    /// Discover the server's tools and register each on `registry`
    /// under its public name. Returns the public names registered.
    pub async fn bridge_tools(
        self: &Arc<Self>,
        registry: &ToolRegistry,
    ) -> Result<Vec<String>, McpError> {
        let _sync = self.sync.lock().await;
        let mut discovered: Vec<Arc<dyn Tool>> = Vec::new();
        let mut cursors = std::collections::HashSet::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.request("tools/list", params).await?;
            let tools = result["tools"].as_array().cloned().unwrap_or_default();
            for t in tools {
                let Some(raw) = t["name"].as_str() else { continue };
                let public = public_tool_name(&self.server, raw);
                if discovered.len() >= 4096 || discovered.iter().any(|tool| tool.name() == public) {
                    return Err(McpError::Protocol { server: self.server.clone(), message: "duplicate tool or catalog exceeds 4096 tools".into() });
                }
                discovered.push(Arc::new(McpTool {
                    images: registry.images.clone(),
                    conn: Arc::clone(self),
                    raw: raw.to_string(),
                    public: public.clone(),
                    description: t["description"].as_str().unwrap_or("").to_string(),
                    schema: if t["inputSchema"].is_object() {
                        t["inputSchema"].clone()
                    } else {
                        json!({ "type": "object" })
                    },
                }));
            }
            match result["nextCursor"].as_str() {
                Some(c) if cursors.len() < 128 && cursors.insert(c.to_owned()) => cursor = Some(c.to_string()),
                Some(_) => return Err(McpError::Protocol { server: self.server.clone(), message: "repeated cursor or too many catalog pages".into() }),
                None => break,
            }
        }
        let mut owned = self.registered.lock().expect("registered lock");
        let previous: Vec<_> = owned.iter().filter_map(|tool| tool.upgrade()).collect();
        // Check conflicts before changing any registration.
        for tool in &discovered {
            if let Some(current) = registry.get(tool.name()) {
                if !previous.iter().any(|old| Arc::ptr_eq(old, &current)) {
                    return Err(McpError::Protocol { server: self.server.clone(), message: format!("tool registration conflict: {}", tool.name()) });
                }
            }
        }
        let mut names = Vec::new();
        for tool in discovered {
            let result = if let Some(old) = previous.iter().find(|old| old.name() == tool.name()) {
                registry.replace_if_current(old, tool.clone())
            } else { registry.try_register(tool.clone()) };
            result.map_err(|message| McpError::Protocol { server: self.server.clone(), message })?;
            names.push(tool.name().to_owned());
            owned.push(Arc::downgrade(&tool));
        }
        for old in previous {
            if !names.iter().any(|name| name == old.name()) { registry.unregister_if_current(&old); }
        }
        owned.retain(|tool| tool.upgrade().is_some_and(|tool| registry.get(tool.name()).is_some_and(|current| Arc::ptr_eq(&tool, &current))));
        if self.deferred.load(Ordering::Acquire) { registry.defer(names.iter().cloned()); }
        Ok(names)
    }

    /// Watch catalog changes. EOF removes stale tools; reconnect is explicit so
    /// a crashed server cannot silently restart side-effecting startup code.
    pub fn tools_deferred(&self) -> bool { self.deferred.load(Ordering::Acquire) }

    /// Exact currently owned registrations, never inferred from a namespace prefix.
    pub fn tool_names(&self, registry: &ToolRegistry) -> Vec<String> {
        let mut names: Vec<_> = self.registered.lock().expect("registered lock").iter()
            .filter_map(|tool| tool.upgrade())
            .filter(|tool| registry.get(tool.name()).is_some_and(|current| Arc::ptr_eq(tool, &current)))
            .map(|tool| tool.name().to_owned()).collect();
        names.sort();
        names.dedup();
        names
    }

    pub async fn watch(self: &Arc<Self>, registry: &Arc<ToolRegistry>, deferred: bool) {
        let _sync = self.sync.lock().await;
        self.deferred.store(deferred, Ordering::Release);
        if deferred { registry.defer(self.tool_names(registry)); }
        let connection = Arc::downgrade(self);
        let registry = Arc::downgrade(registry);
        let changed = self.changed.clone();
        tokio::spawn(async move {
            loop {
                changed.notified().await;
                let (Some(conn), Some(registry)) = (connection.upgrade(), registry.upgrade()) else { break };
                if conn.closed.load(Ordering::Acquire) {
                    conn.disconnect(&registry).await;
                    break;
                }
                match conn.bridge_tools(&registry).await {
                    Ok(_) => {},
                    Err(error) => tracing::warn!(%error, "MCP catalog refresh failed"),
                }
            }
        });
    }

    /// Unregister this connection's tools and kill the child. Idempotent.
    pub async fn disconnect(&self, registry: &ToolRegistry) {
        self.closed.store(true, Ordering::Release);
        self.cancel.cancel();
        if let Some(reader) = self.http_reader.lock().unwrap().take() { reader.abort(); }
        let _sync = self.sync.lock().await;
        if let Some(reader) = &self.reader { reader.abort(); }
        if let Some(reader) = self.http_reader.lock().unwrap().take() { reader.abort(); }
        self.changed.notify_one();
        for tool in self.registered.lock().expect("registered lock").drain(..).filter_map(|tool| tool.upgrade()) {
            registry.unregister_if_current(&tool);
        }
        self.pending.lock().expect("pending lock").clear();
        if let Some(http) = &self.http { http.close().await; }
        let child = self.child.lock().expect("child lock").take();
        if let Some(mut child) = child {
            let _ = child.kill().await;
        }
    }
}

/// One bridged MCP tool. Text content blocks are joined; `isError`
/// results become is_error tool results (turns never abort).
struct McpTool {
    images: Arc<std::sync::OnceLock<Arc<rness_engine::images::ImageStore>>>,
    conn: Arc<McpConnection>,
    raw: String,
    public: String,
    description: String,
    schema: Value,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.public
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute_rich(
        &self,
        session: &rness_protocol::events::SessionId,
        _call: &str,
        args: Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool), String> {
        self.execute_presented(session, &_call.to_owned(), args, cancel).await.map(|(content, tasks, error, _)| (content, tasks, error))
    }

    async fn execute_presented(
        &self, session: &rness_protocol::events::SessionId, _call: &String, args: Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        if cancel.is_cancelled() { return Err("MCP call cancelled before dispatch".into()); }
        let id = self.conn.next_id.fetch_add(1, Ordering::Relaxed);
        let result = tokio::select! {
            _ = cancel.cancelled() => {
                // Best effort only: acknowledgement is not confirmation that
                // remote side effects stopped. Never resend the tools/call.
                let _ = tokio::time::timeout(self.conn.timeout.min(Duration::from_secs(1)),
                    self.conn.notify("notifications/cancelled", json!({"requestId":id, "reason":"caller cancelled"}))).await;
                return Err("MCP call cancelled locally; remote termination is not confirmed".into());
            },
            result = self.conn.request_with_id(id, "tools/call", json!({ "name": self.raw, "arguments": args })) => result.map_err(|e| e.to_string())?,
        };
        let mut metadata = json!({"version":1,"kind":"mcp","tool":self.raw});
        if let Some(structured) = result.get("structuredContent") {
            if serde_json::to_vec(structured).is_ok_and(|bytes| bytes.len() <= 48 * 1024) {
                metadata["structured_content"] = structured.clone();
                metadata["truncated"] = json!(false);
            } else {
                metadata["truncated"] = json!(true);
            }
        }
        let images = self.images.get().cloned();
        let session = session.clone();
        let token = cancel.clone();
        tokio::task::spawn_blocking(move || decode_content(result, images.as_deref(), &session, &token))
            .await.map_err(|e| e.to_string())?
            .map(|(content, tasks, error)| (content, tasks, error, Some(metadata)))
    }

    async fn execute(&self, args: Value) -> Result<String, String> {
        let result = self
            .conn
            .request("tools/call", json!({ "name": self.raw, "arguments": args }))
            .await
            .map_err(|e| e.to_string())?;
        let text: String = result["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| {
                        (b["type"] == "text").then(|| b["text"].as_str().unwrap_or(""))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if result["content"].as_array().is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "image")) {
            return Err(format!("MCP returned image content, but rich tool-image delivery is not configured. Images were not forwarded.\n{text}"));
        }
        if result["isError"].as_bool() == Some(true) {
            Err(if text.is_empty() { "tool reported an error".into() } else { text })
        } else {
            Ok(text)
        }
    }
}

fn decode_content(
    result: Value,
    images: Option<&rness_engine::images::ImageStore>,
    session: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool), String> {
    use base64::Engine;
    use rness_protocol::events::ToolResultContentPart;
    let blocks = result["content"].as_array().ok_or("MCP result has no content array")?;
    let mut content = Vec::new();
    for block in blocks {
        if cancel.is_cancelled() { return Err("MCP call cancelled".into()); }
        match block["type"].as_str() {
            Some("text") => content.push(ToolResultContentPart::Text {
                text: block["text"].as_str().ok_or("MCP text block has no text")?.into(),
            }),
            Some("image") => {
                let store = images.ok_or("MCP image storage is not configured")?;
                let encoded = block["data"].as_str().ok_or("MCP image has no data")?;
                if encoded.len() > store.policy().max_input_bytes.saturating_add(2).saturating_div(3).saturating_mul(4) { return Err("MCP encoded image exceeds input limit".into()); }
                let data = base64::engine::general_purpose::STANDARD.decode(encoded)
                    .map_err(|e| format!("invalid MCP image base64: {e}"))?;
                let mime = block["mimeType"].as_str().ok_or("MCP image has no mimeType")?;
                let attachment = store.admit_for_session(session, &data, mime)?;
                content.push(ToolResultContentPart::Image { attachment });
            }
            other => return Err(format!("unsupported MCP content type: {other:?}")),
        }
    }
    if cancel.is_cancelled() { return Err("MCP call cancelled".into()); }
    Ok((content, None, result["isError"].as_bool().unwrap_or(false)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_results_preserve_order_errors_and_durable_images() {
        use base64::Engine;
        use rness_protocol::events::ToolResultContentPart;
        let dir = tempfile::tempdir().unwrap();
        let store = rness_engine::images::ImageStore::new(dir.path().into(), Default::default()).unwrap();
        let image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let result = json!({"isError":true, "content":[{"type":"text","text":"before"},{"type":"image","mimeType":"image/png","data":image},{"type":"text","text":"after"}]});
        let cancel = tokio_util::sync::CancellationToken::new();
        let (parts, _, error) = decode_content(result.clone(), Some(&store), "session", &cancel).unwrap();
        assert!(error);
        assert!(matches!(&parts[0], ToolResultContentPart::Text { text } if text == "before"));
        let ToolResultContentPart::Image { attachment } = &parts[1] else { panic!("missing image") };
        assert_eq!(store.read(attachment).unwrap(), base64::engine::general_purpose::STANDARD.decode(image).unwrap());
        assert!(store.admitted_for_session("session", &attachment.id));
        assert!(matches!(&parts[2], ToolResultContentPart::Text { text } if text == "after"));
        assert!(decode_content(result.clone(), None, "session", &cancel).is_err());
        cancel.cancel();
        assert!(decode_content(result, Some(&store), "session", &cancel).unwrap_err().contains("cancelled"));
    }

    #[test]
    fn rich_results_reject_invalid_images() {
        let dir = tempfile::tempdir().unwrap();
        let store = rness_engine::images::ImageStore::new(dir.path().into(), Default::default()).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        for data in ["%%%", "bm90IGFuIGltYWdl"] {
            assert!(decode_content(json!({"content":[{"type":"image","mimeType":"image/png","data":data}]}), Some(&store), "s", &cancel).is_err());
        }
    }

    #[test]
    fn public_names_are_verbatim_when_clean() {
        assert_eq!(public_tool_name("github", "create_issue"), "mcp__github__create_issue");
    }

    #[test]
    fn dirty_names_get_stable_hash_suffix() {
        let a = public_tool_name("srv", "weird.name");
        let b = public_tool_name("srv", "weird.name");
        assert_eq!(a, b);
        assert!(a.starts_with("mcp__srv__weird_name_"));
        // Distinct identities never collapse.
        assert_ne!(public_tool_name("srv", "weird.name"), public_tool_name("srv", "weird_name"));
    }

    #[test]
    fn overlong_names_fit_the_budget() {
        let raw = "x".repeat(100);
        let name = public_tool_name("server", &raw);
        assert!(name.len() <= 64);
        assert_eq!(name, public_tool_name("server", &raw));
    }

    #[test]
    fn server_names_are_validated() {
        assert!(valid_server_name("github"));
        assert!(valid_server_name("my-server_2"));
        assert!(!valid_server_name(""));
        assert!(!valid_server_name("has space"));
        assert!(!valid_server_name(&"x".repeat(33)));
    }
}
