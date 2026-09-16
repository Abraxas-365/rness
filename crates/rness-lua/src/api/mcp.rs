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
//!   } -> { "mcp__github__create_issue", ... }  (public tool names)
//!
//!   rness.mcp.disconnect("github") -> true|false
//!   rness.mcp.reconnect("github") -> { "mcp__github__create_issue", ... }
//!   rness.mcp.servers() -> { "github", ... }
//!
//! Blocking on the VM actor (stalls Lua, never the engine) — same
//! stance as rness.http and rness.subagents. Connections live across
//! hot reloads: they belong to the HOST (this table), not the VM.

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
    connection: tokio::sync::Mutex<Arc<McpConnection>>,
    cancel: tokio_util::sync::CancellationToken,
}
impl Drop for ManagedConnection {
    fn drop(&mut self) { self.cancel.cancel(); }
}
pub type McpConnections = Arc<Mutex<HashMap<String, Arc<ManagedConnection>>>>;

fn supervise(managed: &Arc<ManagedConnection>, registry: &Arc<ToolRegistry>, policy: rness_mcp::reconnect::ReconnectPolicy, rt: &tokio::runtime::Handle) {
    if !policy.enabled { return; }
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
            if !managed.connection.lock().await.is_closed() { continue; }
            if attempts >= policy.max_attempts { break; }
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(delay)) => {},
            }
            let mut connection = managed.connection.lock().await;
            if cancel.is_cancelled() { break; }
            if !connection.is_closed() { continue; }
            attempts += 1;
            // Do not cancel catalog installation halfway through; disconnect
            // waits on this mutex and removes any newly installed tools.
            match connection.reconnect(&registry).await {
                Ok(fresh) => {
                    let deferred = connection.tools_deferred();
                    fresh.watch(&registry, deferred).await;
                    *connection = fresh;
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
            let policy: rness_mcp::reconnect::ReconnectPolicy = match spec.get::<Option<Table>>("reconnect")? {
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
            if command.is_some() == url.is_some() { return Err(err("MCP requires exactly one of command or url")); }
            let headers: HashMap<String, String> = spec.get::<Option<HashMap<String, String>>>("headers")?.unwrap_or_default();
            let args: Vec<String> = spec.get::<Option<Vec<String>>>("args")?.unwrap_or_default();
            let env: Vec<(String, String)> = spec
                .get::<Option<HashMap<String, String>>>("env")?
                .unwrap_or_default()
                .into_iter()
                .collect();
            let timeout_ms: u64 = spec.get::<Option<u64>>("timeout_ms")?.unwrap_or(30_000);
            let defer_tools: bool = spec.get::<Option<bool>>("defer_tools")?.unwrap_or(false);

            if conns.lock().expect("mcp lock").contains_key(&name) {
                return Err(err(format!("mcp server '{name}' is already connected")));
            }
            if timeout_ms == 0 { return Err(err("MCP timeout_ms must be positive")); }
            let (conn, tools) = handle
                .block_on(async {
                    let conn = if let Some(url) = url {
                        McpConnection::connect_http(rness_mcp::http::HttpServer {
                            name: name.clone(), url, headers: headers.into_iter().collect(), timeout: Duration::from_millis(timeout_ms), sse,
                        }).await?
                    } else {
                        McpConnection::connect(StdioServer {
                            name: name.clone(), command: command.expect("validated command"), args, env, timeout: Duration::from_millis(timeout_ms),
                        }).await?
                    };
                    let tools = match conn.bridge_tools(&reg).await {
                        Ok(tools) => tools,
                        Err(error) => { conn.disconnect(&reg).await; return Err(error); },
                    };
                    Ok::<_, rness_mcp::McpError>((conn, tools))
                })
                .map_err(err)?;
            handle.block_on(conn.watch(&reg, defer_tools));
            let managed = Arc::new(ManagedConnection { connection: tokio::sync::Mutex::new(conn), cancel: Default::default() });
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
            handle.block_on(async { conn.connection.lock().await.disconnect(&reg).await });
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
    mcp.set("reconnect", lua.create_function(move |_, name: String| {
        let old = conns.lock().expect("mcp lock").get(&name).cloned().ok_or_else(|| err("unknown MCP server"))?;
        handle.block_on(async {
            let mut connection = old.connection.lock().await;
            let deferred = connection.tools_deferred();
            let fresh = connection.reconnect(&reg).await.map_err(err)?;
            fresh.watch(&reg, deferred).await;
            let names = fresh.tool_names(&reg);
            *connection = fresh;
            Ok(names)
        })
    })?)?;

    rness.set("mcp", mcp)?;
    Ok(())
}
