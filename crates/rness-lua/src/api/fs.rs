//! rness.fs — filesystem conveniences.
//!
//! Not a capability fence (there is no sandbox — plugins already have
//! `io.*`): these exist for ergonomics. `~` expands, errors are Lua
//! errors with the path in the message, `list` returns a sorted array.

use mlua::{Lua, Table};

/// Expand a leading `~` to the user's home directory.
fn expand(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

pub fn install(lua: &Lua, rness: &Table) -> Result<(), mlua::Error> {
    let fs = lua.create_table()?;

    // rness.fs.read(path) -> string
    fs.set(
        "read",
        lua.create_function(|_, path: String| {
            std::fs::read_to_string(expand(&path))
                .map_err(|e| mlua::Error::runtime(format!("fs.read('{path}'): {e}")))
        })?,
    )?;

    // rness.fs.write(path, content) — creates parent directories.
    fs.set(
        "write",
        lua.create_function(|_, (path, content): (String, String)| {
            let p = expand(&path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| mlua::Error::runtime(format!("fs.write('{path}'): {e}")))?;
            }
            std::fs::write(&p, content)
                .map_err(|e| mlua::Error::runtime(format!("fs.write('{path}'): {e}")))
        })?,
    )?;

    // rness.fs.exists(path) -> bool
    fs.set(
        "exists",
        lua.create_function(|_, path: String| Ok(expand(&path).exists()))?,
    )?;

    // rness.fs.list(dir) -> { "name", ... } sorted, names only.
    fs.set(
        "list",
        lua.create_function(|_, dir: String| {
            let entries = std::fs::read_dir(expand(&dir))
                .map_err(|e| mlua::Error::runtime(format!("fs.list('{dir}'): {e}")))?;
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            Ok(names)
        })?,
    )?;

    rness.set("fs", fs)?;
    Ok(())
}
