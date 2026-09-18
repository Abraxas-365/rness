//! Non-consuming background job inspection and cancellation for user controls.

use mlua::{Lua, LuaSerdeExt, Table};
use rness_tools::jobs::JobRegistry;

pub fn install_unmounted(lua: &Lua, rness: &Table) -> mlua::Result<()> {
    let jobs = lua.create_table()?;
    for name in ["count", "list", "inspect", "stop"] {
        jobs.set(
            name,
            lua.create_function(|_, _: mlua::MultiValue| -> mlua::Result<()> {
                Err(mlua::Error::runtime("rness.jobs is not installed"))
            })?,
        )?;
    }
    jobs.set(
        "setup",
        lua.create_function(|lua, config: Table| {
            if lua
                .named_registry_value::<Option<bool>>("rness.jobs.startup_closed")?
                .unwrap_or(false)
                || lua
                    .globals()
                    .get::<Option<String>>("__rness_loading_plugin")?
                    .is_some()
                || lua
                    .globals()
                    .get::<Option<Table>>("__rness_messagebox_renderers")?
                    .is_some()
            {
                return Err(mlua::Error::runtime(
                    "jobs.setup is startup-only; configure it in init.lua",
                ));
            }
            let rness: Table = lua.globals().get("rness")?;
            rness.get::<Table>("jobs")?.set("config", config)
        })?,
    )?;
    rness.set("jobs", jobs)
}

pub fn install(lua: &Lua, rness: &Table, registry: JobRegistry) -> mlua::Result<()> {
    // Preserve startup configuration and the stable setup API.
    let jobs = rness.get::<Table>("jobs")?;
    let r = registry.clone();
    jobs.set(
        "count",
        lua.create_function(move |_, session: String| Ok(r.count(&session)))?,
    )?;
    let r = registry.clone();
    jobs.set(
        "list",
        lua.create_function(move |lua, session: String| {
            let out = lua.create_table()?;
            for (i, job) in r.list(&session).into_iter().enumerate() {
                out.set(i + 1, lua.to_value(&job)?)?;
            }
            Ok(out)
        })?,
    )?;
    let r = registry.clone();
    jobs.set(
        "inspect",
        lua.create_function(move |lua, (session, id): (String, String)| {
            let job = r.inspect(&session, &id).map_err(mlua::Error::runtime)?;
            lua.to_value(&job)
        })?,
    )?;
    jobs.set(
        "stop",
        lua.create_function(move |_, (session, id): (String, String)| {
            registry.stop(&session, &id).map_err(mlua::Error::runtime)
        })?,
    )?;
    rness.set("jobs", jobs)
}
