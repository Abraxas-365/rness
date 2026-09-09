//! rness.session — the engine's SessionService surfaced to Lua.
//!
//! Injected AFTER the kernel mounts (the VM boots first), and
//! re-injected into every fresh VM after a hot reload — see
//! [`crate::plugin_host`]. Calls run on the VM actor thread; `send`
//! spawns the turn burst, so the tokio runtime handle captured at
//! install time is entered around it.
//!
//!   rness.session.list()            -> { id, ... }
//!   rness.session.phase(id)         -> "idle" | "running"
//!   rness.session.send(id, text)    -> "started"|"queued"|"logged"
//!   rness.session.fork(id)          -> child id
//!   rness.session.transcript(id)    -> { {role=, text=}, ... }
//!   rness.session.usage(id)         -> { input=, output= } (context tokens)
//!   rness.session.config(id[, cfg]) -> { reasoning=? } (get; with cfg: set —
//!     durable request/config event, resume remembers it)
//!   rness.session.compact(id, keep) -> { shadowed=, summary= } (blocks)
//!   rness.session.prune(id, opts)   -> pruned count (deterministic, instant)
//!     opts = { threshold=, head=, tail=, keep_turns= } (all required — no
//!     hidden defaults; dsh reference: 8192/4096/1024/2)

use mlua::LuaSerdeExt;
use std::sync::Arc;

use mlua::{Lua, Table};
use rness_engine::inbox::{Disposition, Phase};
use rness_engine::service::SessionService;
use rness_protocol::events::{ContentPart, UserIntent};

fn err(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

pub fn install(
    lua: &Lua,
    rness: &Table,
    sessions: Arc<SessionService>,
    rt: tokio::runtime::Handle,
) -> Result<(), mlua::Error> {
    let session = lua.create_table()?;
    let s = Arc::clone(&sessions);
    session.set("tasks", lua.create_function(move |lua, id: String| {
        lua.to_value(&s.tasks(&id).map_err(err)?)
    })?)?;
    let s = Arc::clone(&sessions);
    session.set("agent", lua.create_function(move |lua, (id, name): (String, Option<String>)| {
        if let Some(name) = name { s.select_agent(&id, &name).map_err(err)?; }
        lua.to_value(&s.config(&id).map_err(err)?.agent)
    })?)?;

    let s = Arc::clone(&sessions);
    session.set(
        "list",
        lua.create_function(move |_, ()| s.list().map_err(err))?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "phase",
        lua.create_function(move |_, id: String| {
            Ok(match s.phase(&id) {
                Phase::Idle => "idle",
                Phase::Running => "running",
            })
        })?,
    )?;

    let s = Arc::clone(&sessions);
    let send_rt = rt.clone();
    session.set(
        "send",
        lua.create_function(move |_, (id, text): (String, String)| {
            // send() spawns the turn burst; enter the runtime for it.
            let _guard = send_rt.enter();
            let disposition = s
                .send(&id, UserIntent::Followup, vec![ContentPart::Text { text }])
                .map_err(err)?;
            Ok(match disposition {
                Disposition::Command(_) => "command",
                Disposition::StartTurn => "started",
                Disposition::Queued => "queued",
                Disposition::LogOnly => "logged",
            })
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "fork",
        lua.create_function(move |_, id: String| s.fork(&id, None).map_err(err))?,
    )?;

    // Branch queries — the DAG view over the store.
    let s = Arc::clone(&sessions);
    session.set(
        "parent",
        lua.create_function(move |lua, id: String| {
            match s.store().parent(&id).map_err(err)? {
                None => Ok(mlua::Value::Nil),
                Some(fork) => {
                    let t = lua.create_table()?;
                    t.set("session", fork.session)?;
                    t.set("at", fork.at)?;
                    Ok(mlua::Value::Table(t))
                }
            }
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "children",
        lua.create_function(move |lua, id: String| {
            let kids = s.store().children(&id, None).map_err(err)?;
            let out = lua.create_table()?;
            for k in kids {
                let t = lua.create_table()?;
                t.set("session", k.session)?;
                t.set("at", k.at)?;
                out.push(t)?;
            }
            Ok(out)
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "ancestry",
        lua.create_function(move |lua, id: String| {
            let hops = s.store().ancestry(&id).map_err(err)?;
            let out = lua.create_table()?;
            for h in hops {
                let t = lua.create_table()?;
                t.set("session", h.session)?;
                t.set("forked_at", h.forked_at)?;
                out.push(t)?;
            }
            Ok(out)
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "transcript",
        lua.create_function(move |lua, id: String| {
            use rness_engine::session::projection::TranscriptItem;
            let transcript = s.transcript(&id).map_err(err)?;
            let items = lua.create_table()?;
            for item in &transcript.items {
                let (role, text) = match item {
                    TranscriptItem::User { content, .. } => ("user", text_of(content)),
                    TranscriptItem::Assistant { content, .. } => ("assistant", text_of(content)),
                    TranscriptItem::Tool { result, .. } => ("tool", result.output.clone()),
                    TranscriptItem::Compaction { summary, .. } => ("summary", summary.clone()),
                    TranscriptItem::Attempt { .. } => continue,
                };
                let entry = lua.create_table()?;
                entry.set("role", role)?;
                entry.set("text", text)?;
                items.push(entry)?;
            }
            Ok(items)
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "usage",
        lua.create_function(move |lua, id: String| {
            let replayed = s.replay(&id).map_err(err)?;
            let t = lua.create_table()?;
            t.set("input", replayed.context.usage.input_tokens)?;
            t.set("output", replayed.context.usage.output_tokens)?;
            t.set("turns", replayed.context.turns.len())?;
            Ok(t)
        })?,
    )?;

    // rness.session.config(id)          -> { reasoning=? } (effective)
    // rness.session.config(id, {...})   -> set (durable request/config
    //   event; nil field = clear, back to the provider's default)
    let s = Arc::clone(&sessions);
    session.set(
        "config",
        lua.create_function(move |lua, (id, new): (String, Option<Table>)| {
            if let Some(new) = new {
                let config = mlua::LuaSerdeExt::from_value::<rness_protocol::events::CallConfig>(
                    lua, mlua::Value::Table(new),
                )?;
                s.set_config(&id, config).map_err(err)?;
            }
            let config = s.config(&id).map_err(err)?;
            mlua::LuaSerdeExt::to_value(lua, &config)
        })?,
    )?;

    let s = Arc::clone(&sessions);
    let block_rt = rt.clone();
    session.set(
        "compact",
        lua.create_function(move |lua, (id, keep): (String, Option<usize>)| {
            // Blocks the VM thread through the summarizer request — meant
            // for explicit commands / idle hooks, same stance as
            // subagents.start.
            let report = block_rt
                .block_on(s.compact(&id, keep.unwrap_or(2)))
                .map_err(err)?;
            let t = lua.create_table()?;
            t.set("shadowed", report.shadowed)?;
            t.set("summary", report.summary)?;
            Ok(t)
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "prune",
        lua.create_function(move |_, (id, opts): (String, Table)| {
            let opts = rness_engine::service::PruneOptions {
                threshold_chars: opts.get("threshold")?,
                head_chars: opts.get("head")?,
                tail_chars: opts.get("tail")?,
                keep_turns: opts.get("keep_turns")?,
            };
            s.prune_tool_results(&id, opts).map_err(err)
        })?,
    )?;

    rness.set("session", session)?;
    Ok(())
}

/// Concatenated text parts of a message (thinking/tool_use skipped).
fn text_of(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
