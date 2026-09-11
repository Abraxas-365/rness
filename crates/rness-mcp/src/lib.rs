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

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// A live connection to one MCP server. Dropping it kills the child;
/// `disconnect` additionally unregisters the bridged tools.
pub struct McpConnection {
    server: String,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    timeout: Duration,
    child: Mutex<Option<Child>>,
    /// Public names registered on behalf of this connection.
    registered: Mutex<Vec<String>>,
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

        // Reader task: route responses to waiting callers by id.
        // Server-initiated requests/notifications are ignored (tools
        // bridge only).
        let route = Arc::clone(&pending);
        let server_name = spec.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    tracing::warn!(server = %server_name, "mcp: unparseable line");
                    continue;
                };
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
            // EOF: fail everything still waiting.
            for (_, tx) in route.lock().expect("pending lock").drain() {
                let _ = tx.send(Err("connection closed".into()));
            }
        });

        let conn = Arc::new(Self {
            server: spec.name,
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            pending,
            next_id: AtomicU64::new(1),
            timeout: spec.timeout,
            child: Mutex::new(Some(child)),
            registered: Mutex::new(Vec::new()),
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

    pub fn server(&self) -> &str {
        &self.server
    }

    async fn send_raw(&self, payload: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(payload).expect("serializable");
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|_| McpError::Closed { server: self.server.clone() })?;
        stdin.flush().await.map_err(|_| McpError::Closed { server: self.server.clone() })
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.send_raw(&json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    /// One JSON-RPC request with the connection's timeout.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        self.send_raw(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        let timeout_ms = self.timeout.as_millis() as u64;
        match tokio::time::timeout(self.timeout, rx).await {
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(McpError::Timeout { server: self.server.clone(), timeout_ms })
            }
            Ok(Err(_)) => Err(McpError::Closed { server: self.server.clone() }),
            Ok(Ok(Err(message))) => Err(McpError::Protocol { server: self.server.clone(), message }),
            Ok(Ok(Ok(result))) => Ok(result),
        }
    }

    /// Discover the server's tools and register each on `registry`
    /// under its public name. Returns the public names registered.
    pub async fn bridge_tools(
        self: &Arc<Self>,
        registry: &ToolRegistry,
    ) -> Result<Vec<String>, McpError> {
        let mut names = Vec::new();
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
                registry.register(Arc::new(McpTool {
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
                names.push(public);
            }
            match result["nextCursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }
        *self.registered.lock().expect("registered lock") = names.clone();
        Ok(names)
    }

    /// Unregister this connection's tools and kill the child. Idempotent.
    pub async fn disconnect(&self, registry: &ToolRegistry) {
        for name in self.registered.lock().expect("registered lock").drain(..) {
            registry.unregister(&name);
        }
        if let Some(mut child) = self.child.lock().expect("child lock").take() {
            let _ = child.start_kill();
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
        let result = tokio::select! {
            _ = cancel.cancelled() => return Err("MCP call cancelled".into()),
            result = self.conn.request("tools/call", json!({ "name": self.raw, "arguments": args })) => result.map_err(|e| e.to_string())?,
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
