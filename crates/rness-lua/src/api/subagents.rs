//! rness.subagents — delegation from Lua (peer of the model's tool).
//!
//! Blocking by design: `start` parks the VM actor thread on the child's
//! completion via the tokio handle — Lua stalls, the engine doesn't
//! (same stance as rness.http). Plugins wanting concurrency start
//! several children from several key presses, or use the session API.
//!
//!   rness.subagents.providers()          -> { "fork", "spawn" }
//!   rness.subagents.start(provider, {parent=, prompt=})
//!       -> { session=, stop="completed"|"aborted"|"error", output= }
//!   rness.subagents.delegation(id)       -> {parent=, depth=, mode=} | nil
//!   rness.subagents.start_continuable(provider, {parent=, prompt=}) -> child id
//!   rness.subagents.send_message(sender, target, text)
//!   rness.subagents.interrupt(caller, target)
//!   rness.subagents.children(root, scope?) -> { {session=, parent=, depth=, running=} }

use std::sync::Arc;

use mlua::{Lua, LuaSerdeExt, Table};
use rness_engine::subagent::{StopReason, SubagentRequest, SubagentRuntime};

fn err(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

pub fn install(
    lua: &Lua,
    rness: &Table,
    runtime: Arc<SubagentRuntime>,
    sessions: Arc<rness_engine::service::SessionService>,
    rt: tokio::runtime::Handle,
) -> Result<(), mlua::Error> {
    let subagents = lua.create_table()?;
    let r = Arc::clone(&runtime);
    subagents.set("roster", lua.create_function(move |lua, ()| lua.to_value(&r.roster()))?)?;

    let r = Arc::clone(&runtime);
    subagents.set(
        "providers",
        lua.create_function(move |_, ()| Ok(r.provider_names()))?,
    )?;

    let r = Arc::clone(&runtime);
    let block_rt = rt.clone();
    subagents.set(
        "start",
        lua.create_function(move |lua, (provider, spec): (String, Table)| {
            let request = SubagentRequest {
                agent: spec.get("agent")?,
                parent: spec.get("parent")?,
                prompt: spec.get("prompt")?,
            };
            let run = block_rt
                .block_on(r.start(&provider, request))
                .map_err(err)?;
            let t = lua.create_table()?;
            t.set("session", run.session)?;
            t.set(
                "stop",
                match run.stop {
                    StopReason::Completed => "completed",
                    StopReason::Aborted => "aborted",
                    StopReason::Error => "error",
                },
            )?;
            t.set("output", run.output)?;
            Ok(t)
        })?,
    )?;

    subagents.set(
        "delegation",
        lua.create_function(move |lua, id: String| {
            match sessions.store().delegation(&id).map_err(err)? {
                None => Ok(mlua::Value::Nil),
                Some(d) => {
                    let t = lua.create_table()?;
                    t.set("parent", d.parent)?;
                    t.set("depth", d.depth)?;
                    t.set(
                        "mode",
                        match d.mode {
                            rness_protocol::branch::DelegationMode::OneShot => "one_shot",
                            rness_protocol::branch::DelegationMode::Continuable => "continuable",
                        },
                    )?;
                    Ok(mlua::Value::Table(t))
                }
            }
        })?,
    )?;

    // start_continuable / send_message may start a turn task
    // (tokio::spawn) — enter the runtime so the VM actor thread has a
    // reactor context.
    let r = Arc::clone(&runtime);
    let handle = rt.clone();
    subagents.set(
        "start_continuable",
        lua.create_function(move |_, (provider, spec): (String, Table)| {
            let request = SubagentRequest {
                agent: spec.get("agent")?,
                parent: spec.get("parent")?,
                prompt: spec.get("prompt")?,
            };
            let _g = handle.enter();
            r.start_continuable(&provider, request).map_err(err)
        })?,
    )?;

    let r = Arc::clone(&runtime);
    let handle = rt.clone();
    subagents.set(
        "send_message",
        lua.create_function(move |_, (sender, target, text): (String, String, String)| {
            let _g = handle.enter();
            r.send_message(&sender, &target, text).map_err(err)?;
            Ok(())
        })?,
    )?;

    let r = Arc::clone(&runtime);
    subagents.set(
        "interrupt",
        lua.create_function(move |_, (caller, target): (String, String)| {
            r.interrupt(&caller, &target).map_err(err)
        })?,
    )?;

    let r = Arc::clone(&runtime);
    subagents.set(
        "children",
        lua.create_function(move |lua, (root, scope): (String, Option<String>)| {
            let descendants = scope.as_deref() == Some("descendants");
            let list = r.list_children(&root, descendants).map_err(err)?;
            let out = lua.create_table()?;
            for (i, c) in list.into_iter().enumerate() {
                let t = lua.create_table()?;
                t.set("session", c.session)?;
                t.set("parent", c.parent)?;
                t.set("depth", c.depth)?;
                t.set("running", c.running)?;
                out.set(i + 1, t)?;
            }
            Ok(out)
        })?,
    )?;

    let r = Arc::clone(&runtime);
    subagents.set("list", lua.create_function(move |lua, root: String| {
        let out = lua.create_table()?;
        for (i, child) in r.list_agents(&root).map_err(err)?.into_iter().enumerate() {
            out.set(i + 1, lua.to_value(&serde_json::json!({
                "session": child.session, "parent": child.parent,
                "depth": child.depth, "running": child.running,
            }))?)?;
        }
        Ok(out)
    })?)?;

    rness.set("subagents", subagents)?;
    Ok(())
}
