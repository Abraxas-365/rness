//! rness.mcp — connect MCP servers from Lua (the composition seam).
//!
//! ZERO config magic: nothing connects unless a plugin calls connect.
//! The user's init.lua IS the mcp config file (dsh does this in
//! cordis.yml; our composition root is Lua).
//!
//!   rness.mcp.connect{
//!     name = "github",            -- namespace: mcp__github__<tool>
//!     command = "npx",
//!     args = {"-y", "@modelcontextprotocol/server-github"},
//!     env = { GITHUB_TOKEN = "..." },   -- optional
//!     defer_tools = true,                 -- optional, default false
//!     timeout_ms = 30000,                 -- optional, default 30s
//!     background = true,                  -- optional, default false
//!   } -> { "mcp__github__create_issue", ... }  (public tool names)
//!
//!   rness.mcp.disconnect("github") -> true|false
//!   rness.mcp.reconnect("github") -> { "mcp__github__create_issue", ... }
//!   rness.mcp.servers() -> { "github", ... }
//!
//! Blocking on the VM actor (stalls Lua, never the engine) — same
//! stance as rness.http and rness.subagents. Connections live across
//! hot reloads: they belong to the HOST (this table), not the VM.
//!
//! `background = true` returns `{}` at once and finishes the handshake on
//! the async runtime, so slow servers no longer delay startup; tools appear
//! in the registry when ready, failures are logged. The server is listed by
//! `servers()` (and rejected as a duplicate) from the moment of the call;
//! `disconnect`/`reconnect` wait for an in-flight handshake.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mlua::{Lua, LuaSerdeExt, Table};
use rness_engine::tools::ToolRegistry;
use rness_mcp::{McpConnection, StdioServer};

fn err(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

/// Host-owned connection set — survives VM swaps.
pub struct ManagedConnection {
    /// `None` while a background handshake is in flight (it holds the lock)
    /// or after one failed.
    connection: tokio::sync::Mutex<Option<Arc<McpConnection>>>,
    cancel: tokio_util::sync::CancellationToken,
}
impl Drop for ManagedConnection {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
pub type McpConnections = Arc<Mutex<HashMap<String, Arc<ManagedConnection>>>>;

enum Server {
    Stdio(StdioServer),
    Http(rness_mcp::http::HttpServer),
}

/// Handshake, bridge the catalog and start watching for list changes.
async fn open(
    server: Server,
    registry: &Arc<ToolRegistry>,
    defer_tools: bool,
) -> Result<(Arc<McpConnection>, Vec<String>), rness_mcp::McpError> {
    let conn = match server {
        Server::Stdio(spec) => McpConnection::connect(spec).await?,
        Server::Http(spec) => McpConnection::connect_http(spec).await?,
    };
    let tools = match conn.bridge_tools(registry).await {
        Ok(tools) => tools,
        Err(error) => {
            conn.disconnect(registry).await;
            return Err(error);
        }
    };
    conn.watch(registry, defer_tools).await;
    Ok((conn, tools))
}

/// Background connect: hold the slot while handshaking so disconnect and
/// reconnect wait; give up quietly if disconnected meanwhile.
#[allow(clippy::too_many_arguments)]
fn connect_in_background(
    name: String,
    server: Server,
    defer_tools: bool,
    managed: Arc<ManagedConnection>,
    connections: McpConnections,
    registry: Arc<ToolRegistry>,
    policy: rness_mcp::reconnect::ReconnectPolicy,
    rt: tokio::runtime::Handle,
) {
    let handle = rt.clone();
    rt.spawn(async move {
        let mut slot = managed.connection.lock().await;
        if managed.cancel.is_cancelled() {
            return;
        }
        match open(server, &registry, defer_tools).await {
            Ok((conn, tools)) => {
                if managed.cancel.is_cancelled() {
                    conn.disconnect(&registry).await;
                    return;
                }
                tracing::info!(server = %name, tools = tools.len(), "MCP connected in background");
                *slot = Some(conn);
                drop(slot);
                supervise(&managed, &registry, policy, &handle);
            }
            Err(error) => {
                tracing::warn!(server = %name, %error, "MCP background connect failed");
                drop(slot);
                let mut conns = connections.lock().expect("mcp lock");
                if conns.get(&name).is_some_and(|m| Arc::ptr_eq(m, &managed)) {
                    conns.remove(&name);
                }
            }
        }
    });
}

fn supervise(
    managed: &Arc<ManagedConnection>,
    registry: &Arc<ToolRegistry>,
    policy: rness_mcp::reconnect::ReconnectPolicy,
    rt: &tokio::runtime::Handle,
) {
    if !policy.enabled {
        return;
    }
    let weak = Arc::downgrade(managed);
    let registry = Arc::downgrade(registry);
    let cancel = managed.cancel.clone();
    rt.spawn(async move {
        // A bounded budget per explicit connect, not reset by a short-lived
        // successful handshake (otherwise a crash loop retries forever).
        let mut attempts = 0;
        let mut delay = policy.initial_delay_ms;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {},
            }
            let (Some(managed), Some(registry)) = (weak.upgrade(), registry.upgrade()) else { break; };
            if !managed.connection.lock().await.as_ref().is_some_and(|c| c.is_closed()) { continue; }
            if attempts >= policy.max_attempts { break; }
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(delay)) => {},
            }
            let mut slot = managed.connection.lock().await;
            if cancel.is_cancelled() { break; }
            let Some(connection) = slot.as_ref() else { break; };
            if !connection.is_closed() { continue; }
            attempts += 1;
            // Do not cancel catalog installation halfway through; disconnect
            // waits on this mutex and removes any newly installed tools.
            match connection.reconnect(&registry).await {
                Ok(fresh) => {
                    let deferred = connection.tools_deferred();
                    fresh.watch(&registry, deferred).await;
                    *slot = Some(fresh);
                },
                Err(error) => tracing::warn!(%error, attempts, "MCP reconnect failed; tool calls are not replayed"),
            }
            delay = delay.saturating_mul(2).min(policy.max_delay_ms);
        }
    });
}

pub fn install(
    lua: &Lua,
    rness: &Table,
    registry: Arc<ToolRegistry>,
    connections: McpConnections,
    rt: tokio::runtime::Handle,
) -> Result<(), mlua::Error> {
    let mcp = lua.create_table()?;

    let conns = Arc::clone(&connections);
    let reg = Arc::clone(&registry);
    let handle = rt.clone();
    mcp.set(
        "connect",
        lua.create_function(move |lua, spec: Table| {
            let policy: rness_mcp::reconnect::ReconnectPolicy =
                match spec.get::<Option<Table>>("reconnect")? {
                    Some(table) => lua.from_value(mlua::Value::Table(table))?,
                    None => Default::default(),
                };
            policy.validate().map_err(err)?;
            let sse: rness_mcp::http::SsePolicy = match spec.get::<Option<Table>>("sse")? {
                Some(table) => lua.from_value(mlua::Value::Table(table))?,
                None => Default::default(),
            };
            sse.validate().map_err(err)?;
            let name: String = spec.get("name")?;
            let command: Option<String> = spec.get("command")?;
            let url: Option<String> = spec.get("url")?;
            if command.is_some() == url.is_some() {
                return Err(err("MCP requires exactly one of command or url"));
            }
            let headers: HashMap<String, String> = spec
                .get::<Option<HashMap<String, String>>>("headers")?
                .unwrap_or_default();
            let args: Vec<String> = spec.get::<Option<Vec<String>>>("args")?.unwrap_or_default();
            let env: Vec<(String, String)> = spec
                .get::<Option<HashMap<String, String>>>("env")?
                .unwrap_or_default()
                .into_iter()
                .collect();
            let timeout_ms: u64 = spec.get::<Option<u64>>("timeout_ms")?.unwrap_or(30_000);
            let defer_tools: bool = spec.get::<Option<bool>>("defer_tools")?.unwrap_or(false);
            let background: bool = spec.get::<Option<bool>>("background")?.unwrap_or(false);

            if conns.lock().expect("mcp lock").contains_key(&name) {
                return Err(err(format!("mcp server '{name}' is already connected")));
            }
            if timeout_ms == 0 {
                return Err(err("MCP timeout_ms must be positive"));
            }
            let server = if let Some(url) = url {
                Server::Http(rness_mcp::http::HttpServer {
                    name: name.clone(),
                    url,
                    headers: headers.into_iter().collect(),
                    timeout: Duration::from_millis(timeout_ms),
                    sse,
                })
            } else {
                Server::Stdio(StdioServer {
                    name: name.clone(),
                    command: command.expect("validated command"),
                    args,
                    env,
                    timeout: Duration::from_millis(timeout_ms),
                })
            };
            if background {
                let managed = Arc::new(ManagedConnection {
                    connection: tokio::sync::Mutex::new(None),
                    cancel: Default::default(),
                });
                {
                    let mut map = conns.lock().expect("mcp lock");
                    if map.contains_key(&name) {
                        return Err(err(format!("mcp server '{name}' is already connected")));
                    }
                    map.insert(name.clone(), Arc::clone(&managed));
                }
                connect_in_background(
                    name,
                    server,
                    defer_tools,
                    managed,
                    Arc::clone(&conns),
                    Arc::clone(&reg),
                    policy,
                    handle.clone(),
                );
                return Ok(Vec::new());
            }
            let (conn, tools) = handle
                .block_on(open(server, &reg, defer_tools))
                .map_err(err)?;
            let managed = Arc::new(ManagedConnection {
                connection: tokio::sync::Mutex::new(Some(conn)),
                cancel: Default::default(),
            });
            supervise(&managed, &reg, policy, &handle);
            conns.lock().expect("mcp lock").insert(name, managed);
            Ok(tools)
        })?,
    )?;

    let conns = Arc::clone(&connections);
    let reg = Arc::clone(&registry);
    let handle = rt.clone();
    mcp.set(
        "disconnect",
        lua.create_function(move |_, name: String| {
            let Some(conn) = conns.lock().expect("mcp lock").remove(&name) else {
                return Ok(false);
            };
            conn.cancel.cancel();
            handle.block_on(async {
                if let Some(connection) = conn.connection.lock().await.take() {
                    connection.disconnect(&reg).await;
                }
            });
            Ok(true)
        })?,
    )?;

    let conns = Arc::clone(&connections);
    mcp.set(
        "servers",
        lua.create_function(move |_, ()| {
            let mut v: Vec<String> = conns.lock().expect("mcp lock").keys().cloned().collect();
            v.sort();
            Ok(v)
        })?,
    )?;

    let conns = Arc::clone(&connections);
    let reg = Arc::clone(&registry);
    let handle = rt.clone();
    mcp.set(
        "reconnect",
        lua.create_function(move |_, name: String| {
            let old = conns
                .lock()
                .expect("mcp lock")
                .get(&name)
                .cloned()
                .ok_or_else(|| err("unknown MCP server"))?;
            handle.block_on(async {
                let mut slot = old.connection.lock().await;
                let connection = slot
                    .as_ref()
                    .ok_or_else(|| err(format!("mcp server '{name}' failed to connect")))?;
                let deferred = connection.tools_deferred();
                let fresh = connection.reconnect(&reg).await.map_err(err)?;
                fresh.watch(&reg, deferred).await;
                let names = fresh.tool_names(&reg);
                *slot = Some(fresh);
                Ok(names)
            })
        })?,
    )?;

    rness.set("mcp", mcp)?;
    Ok(())
}
