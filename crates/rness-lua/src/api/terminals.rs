//! Non-consuming terminal inspection and control for user controls
//! (`/terminals`, the statusline). Reads never move the model's cursor.

use mlua::{Lua, LuaSerdeExt, Table};
use rness_tools::terminal::TerminalRegistry;

const FUNCTIONS: [&str; 5] = ["count", "list", "inspect", "stop", "close"];
/// Scrollback lines `inspect` returns unless asked otherwise.
const DEFAULT_LINES: usize = 40;
const MAX_LINES: usize = 500;

pub fn install_unmounted(lua: &Lua, rness: &Table) -> mlua::Result<()> {
    let terminals = lua.create_table()?;
    for name in FUNCTIONS {
        terminals.set(
            name,
            lua.create_function(|_, _: mlua::MultiValue| -> mlua::Result<()> {
                Err(mlua::Error::runtime("rness.terminals is not installed"))
            })?,
        )?;
    }
    rness.set("terminals", terminals)
}

pub fn install(lua: &Lua, rness: &Table, registry: TerminalRegistry) -> mlua::Result<()> {
    let terminals = rness.get::<Table>("terminals")?;
    let r = registry.clone();
    terminals.set(
        "count",
        lua.create_function(move |lua, session: String| {
            let list = r.list(&session);
            let out = lua.create_table()?;
            out.set("open", list.len())?;
            out.set("running", list.iter().filter(|t| t.running).count())?;
            Ok(out)
        })?,
    )?;
    let r = registry.clone();
    terminals.set(
        "list",
        lua.create_function(move |lua, session: String| {
            let out = lua.create_table()?;
            for (i, terminal) in r.list(&session).into_iter().enumerate() {
                out.set(i + 1, lua.to_value(&terminal)?)?;
            }
            Ok(out)
        })?,
    )?;
    let r = registry.clone();
    terminals.set(
        "inspect",
        lua.create_function(
            move |lua, (session, id, lines): (String, String, Option<usize>)| {
                let lines = lines.unwrap_or(DEFAULT_LINES).min(MAX_LINES);
                let inspection = r
                    .inspect(&id, &session, lines)
                    .map_err(mlua::Error::runtime)?;
                lua.to_value(&inspection)
            },
        )?,
    )?;
    // The VM is single-threaded, so `stop` must not block it through the
    // signal escalation (up to ~3 s): it checks, then escalates on a
    // thread. `close` is bounded by the short hangup grace.
    let r = registry.clone();
    terminals.set(
        "stop",
        lua.create_function(move |_, (session, id): (String, String)| {
            let running = r
                .list(&session)
                .into_iter()
                .find(|t| t.id == id)
                .map(|t| t.running);
            match running {
                // Surface the not-found / ownership error.
                None => r
                    .inspect(&id, &session, 0)
                    .map(|_| false)
                    .map_err(mlua::Error::runtime),
                Some(false) => Ok(false),
                Some(true) => {
                    let r = r.clone();
                    std::thread::Builder::new()
                        .name("terminal-stop".into())
                        .spawn(move || {
                            let _ = r.stop(&id, &session);
                        })
                        .map_err(mlua::Error::external)?;
                    Ok(true)
                }
            }
        })?,
    )?;
    terminals.set(
        "close",
        lua.create_function(move |_, (session, id): (String, String)| {
            registry
                .close(&id, &session)
                .map_err(mlua::Error::runtime)?;
            Ok(true)
        })?,
    )?;
    rness.set("terminals", terminals)
}
