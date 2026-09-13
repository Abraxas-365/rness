//! Non-consuming background job inspection and cancellation for user controls.

use mlua::{Lua, LuaSerdeExt, Table};
use rness_tools::jobs::JobRegistry;

pub fn install_unmounted(lua: &Lua, rness: &Table) -> mlua::Result<()> {
    let jobs = lua.create_table()?;
    for name in ["count", "list", "inspect", "stop"] {
        jobs.set(name, lua.create_function(|_, _: mlua::MultiValue| -> mlua::Result<()> {
            Err(mlua::Error::runtime("rness.jobs is not installed"))
        })?)?;
    }
    rness.set("jobs", jobs)
}

pub fn install(lua: &Lua, rness: &Table, registry: JobRegistry) -> mlua::Result<()> {
    let jobs = lua.create_table()?;
    let r = registry.clone();
    jobs.set("count", lua.create_function(move |_, session: String| {
        Ok(r.count(&session))
    })?)?;
    let r = registry.clone();
    jobs.set("list", lua.create_function(move |lua, session: String| {
        let out = lua.create_table()?;
        for (i, job) in r.list(&session).into_iter().enumerate() {
            out.set(i + 1, lua.to_value(&job)?)?;
        }
        Ok(out)
    })?)?;
    let r = registry.clone();
    jobs.set("inspect", lua.create_function(move |lua, (session, id): (String, String)| {
        let job = r.inspect(&session, &id).map_err(mlua::Error::runtime)?;
        lua.to_value(&job)
    })?)?;
    jobs.set("stop", lua.create_function(move |_, (session, id): (String, String)| {
        registry.stop(&session, &id).map_err(mlua::Error::runtime)
    })?)?;
    rness.set("jobs", jobs)
}
