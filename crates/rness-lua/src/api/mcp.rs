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
//!   rness.mcp.servers() -> { "github", ... }
//!
//! Blocking on the VM actor (stalls Lua, never the engine) — same
//! stance as rness.http and rness.subagents. Connections live across
//! hot reloads: they belong to the HOST (this table), not the VM.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mlua::{Lua, Table};
use rness_engine::tools::ToolRegistry;
use rness_mcp::{McpConnection, StdioServer};

fn err(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

/// Host-owned connection set — survives VM swaps.
pub type McpConnections = Arc<Mutex<HashMap<String, Arc<McpConnection>>>>;

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
        lua.create_function(move |_, spec: Table| {
            let name: String = spec.get("name")?;
            let command: String = spec.get("command")?;
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
            let server = StdioServer {
                name: name.clone(),
                command,
                args,
                env,
                timeout: Duration::from_millis(timeout_ms),
            };
            let (conn, tools) = handle
                .block_on(async {
                    let conn = McpConnection::connect(server).await?;
                    let tools = conn.bridge_tools(&reg).await?;
                    Ok::<_, rness_mcp::McpError>((conn, tools))
                })
                .map_err(err)?;
            if defer_tools {
                reg.defer(tools.iter().cloned());
            }
            conns.lock().expect("mcp lock").insert(name, conn);
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
            handle.block_on(conn.disconnect(&reg));
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

    rness.set("mcp", mcp)?;
    Ok(())
}
