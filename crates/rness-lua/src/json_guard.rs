//! Depth-guarded Lua <-> serde conversion.
//!
//! mlua's serde bridge recurses in Rust once per nesting level with no
//! limit, and so do `serde_json::Value`'s `Drop`, `Clone` and
//! `to_string`. A plugin returning a few thousand nested tables used to
//! overflow the `lua-vm` thread stack and abort the whole process, even
//! inside `pcall` (a native stack overflow is not a Lua error). Every
//! Lua -> Rust conversion goes through [`LuaJsonExt::from_value_guarded`],
//! which first walks the value *iteratively* and rejects too-deep or
//! cyclic tables with an ordinary (catchable) Lua error, before any
//! recursive code runs.
use std::collections::HashSet;
use std::os::raw::c_void;

use mlua::{Lua, LuaSerdeExt, Table, Value as LuaValue};
use serde::de::DeserializeOwned;

/// Deepest Lua table nesting accepted by a conversion (the outermost
/// table is level 1). Equal to serde_json's own decode limit, far below
/// the native-stack overflow point, and leaves headroom for the
/// recursive `Drop`/`to_string` of the resulting `serde_json::Value`.
pub(crate) const MAX_JSON_DEPTH: usize = 128;

/// Deepest `serde_json::Value` nesting handed to Lua. Values parsed by
/// serde_json are at most 128 deep and values built from Lua are capped
/// at [`MAX_JSON_DEPTH`]; this only stops pathological Values built in
/// Rust. Larger than [`MAX_JSON_DEPTH`] so existing deep-but-sane hook
/// payloads keep working.
pub(crate) const MAX_TO_LUA_DEPTH: usize = 512;

/// Upper bound on table visits during one walk. Shared subtables are
/// visited once per reference (as the serde bridge does), so a small DAG
/// can expand exponentially; this bounds the CPU spent on it.
pub(crate) const MAX_TABLE_VISITS: usize = 1_000_000;

fn too_deep() -> mlua::Error {
    mlua::Error::runtime(format!(
        "table nesting exceeds {MAX_JSON_DEPTH} levels (cannot be converted to JSON)"
    ))
}

/// Reject `value` if it is (or contains) a table nested deeper than
/// [`MAX_JSON_DEPTH`], a cycle, or more than [`MAX_TABLE_VISITS`] table
/// visits. Iterative: never recurses on the native stack. Only tables are
/// descended into (keys and values, raw access, like the serde bridge).
pub(crate) fn check_depth(value: &LuaValue) -> mlua::Result<()> {
    let LuaValue::Table(root) = value else {
        return Ok(());
    };
    enum Step {
        Enter(Table, usize),
        Leave(*const c_void),
    }
    let mut on_path: HashSet<*const c_void> = HashSet::new();
    let mut stack = vec![Step::Enter(root.clone(), 1)];
    let mut visits = 0usize;
    while let Some(step) = stack.pop() {
        let (table, depth) = match step {
            Step::Leave(ptr) => {
                on_path.remove(&ptr);
                continue;
            }
            Step::Enter(table, depth) => (table, depth),
        };
        let ptr = table.to_pointer();
        if on_path.contains(&ptr) {
            return Err(mlua::Error::runtime(
                "recursive table detected (cannot be converted to JSON)",
            ));
        }
        if depth > MAX_JSON_DEPTH {
            return Err(too_deep());
        }
        visits += 1;
        if visits > MAX_TABLE_VISITS {
            return Err(mlua::Error::runtime(format!(
                "table too large to convert to JSON (over {MAX_TABLE_VISITS} nested tables)"
            )));
        }
        on_path.insert(ptr);
        stack.push(Step::Leave(ptr));
        table.for_each::<LuaValue, LuaValue>(|k, v| {
            if let LuaValue::Table(t) = k {
                stack.push(Step::Enter(t, depth + 1));
            }
            if let LuaValue::Table(t) = v {
                stack.push(Step::Enter(t, depth + 1));
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// Nesting depth of a `serde_json::Value` (scalars are 0, `{}`/`[]` are
/// 1), computed iteratively; stops counting once `limit` is exceeded.
pub(crate) fn json_depth_exceeds(value: &serde_json::Value, limit: usize) -> bool {
    fn is_container(v: &serde_json::Value) -> bool {
        matches!(
            v,
            serde_json::Value::Array(_) | serde_json::Value::Object(_)
        )
    }
    if !is_container(value) {
        return false;
    }
    // Only containers are pushed; `depth` is the container's own level.
    let mut stack = vec![(value, 1usize)];
    while let Some((v, depth)) = stack.pop() {
        if depth > limit {
            return true;
        }
        match v {
            serde_json::Value::Array(a) => {
                stack.extend(a.iter().filter(|c| is_container(c)).map(|c| (c, depth + 1)))
            }
            serde_json::Value::Object(o) => stack.extend(
                o.values()
                    .filter(|c| is_container(c))
                    .map(|c| (c, depth + 1)),
            ),
            _ => {}
        }
    }
    false
}

/// Guarded conversions; use these instead of `LuaSerdeExt::from_value`
/// for anything plugin-controlled.
pub(crate) trait LuaJsonExt {
    /// `check_depth` + `from_value`: a too-deep or cyclic table is a
    /// normal Lua error, never a native stack overflow.
    #[allow(clippy::wrong_self_convention)] // mirrors LuaSerdeExt::from_value
    fn from_value_guarded<T: DeserializeOwned>(&self, value: LuaValue) -> mlua::Result<T>;
    /// `serde_json::Value` -> Lua with an iterative depth check first.
    fn json_to_lua(&self, value: &serde_json::Value) -> mlua::Result<LuaValue>;
}

impl LuaJsonExt for Lua {
    fn from_value_guarded<T: DeserializeOwned>(&self, value: LuaValue) -> mlua::Result<T> {
        check_depth(&value)?;
        LuaSerdeExt::from_value(self, value)
    }

    fn json_to_lua(&self, value: &serde_json::Value) -> mlua::Result<LuaValue> {
        if json_depth_exceeds(value, MAX_TO_LUA_DEPTH) {
            return Err(mlua::Error::runtime(format!(
                "value nesting exceeds {MAX_TO_LUA_DEPTH} levels (cannot be passed to Lua)"
            )));
        }
        self.to_value(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested(lua: &Lua, levels: usize) -> LuaValue {
        // `levels` tables in total: the root plus levels-1 children.
        lua.load(format!(
            "local t = {{}} local c = t for i = 2, {levels} do c.n = {{}} c = c.n end return t"
        ))
        .eval()
        .unwrap()
    }

    #[test]
    fn depth_limit_is_exact() {
        let lua = Lua::new();
        let ok: serde_json::Value = lua
            .from_value_guarded(nested(&lua, MAX_JSON_DEPTH))
            .unwrap();
        assert!(ok.is_object());
        let err = lua
            .from_value_guarded::<serde_json::Value>(nested(&lua, MAX_JSON_DEPTH + 1))
            .unwrap_err();
        assert!(err.to_string().contains("nesting exceeds 128"), "{err}");
    }

    #[test]
    fn very_deep_table_errors_without_overflow() {
        let lua = Lua::new();
        let err = lua
            .from_value_guarded::<serde_json::Value>(nested(&lua, 100_000))
            .unwrap_err();
        assert!(err.to_string().contains("nesting exceeds"), "{err}");
    }

    #[test]
    fn cycles_are_rejected() {
        let lua = Lua::new();
        for src in [
            "local t = {} t.self = t return t",
            "local a, b = {}, {} a.b = b b.a = a return {a}",
            "local t = {} t[t] = 1 return t",
        ] {
            let v: LuaValue = lua.load(src).eval().unwrap();
            let err = check_depth(&v).unwrap_err();
            assert!(err.to_string().contains("recursive table"), "{src}: {err}");
        }
    }

    #[test]
    fn shared_subtables_and_wide_tables_are_fine() {
        let lua = Lua::new();
        let v: LuaValue = lua
            .load("local s = {1} local t = {} for i = 1, 100000 do t[i] = (i % 2 == 0) and s or i end return {a = s, b = s, list = t}")
            .eval()
            .unwrap();
        let j: serde_json::Value = lua.from_value_guarded(v).unwrap();
        assert_eq!(j["list"].as_array().unwrap().len(), 100_000);
    }

    #[test]
    fn exponential_dag_hits_visit_cap() {
        let lua = Lua::new();
        // 40 levels, each referencing the next twice: 2^40 visits.
        let v: LuaValue = lua
            .load("local c = {} for i = 1, 40 do c = {c, c} end return c")
            .eval()
            .unwrap();
        let err = check_depth(&v).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn scalars_pass_through() {
        let lua = Lua::new();
        let j: serde_json::Value = lua.from_value_guarded(LuaValue::Integer(3)).unwrap();
        assert_eq!(j, serde_json::json!(3));
    }

    #[test]
    fn json_to_lua_depth_guard() {
        let lua = Lua::new();
        let deep = |n: usize| (0..n).fold(serde_json::json!(1), |acc, _| serde_json::json!([acc]));
        assert!(lua.json_to_lua(&deep(MAX_TO_LUA_DEPTH)).is_ok());
        assert!(lua.json_to_lua(&deep(MAX_TO_LUA_DEPTH + 1)).is_err());
        assert!(!json_depth_exceeds(&serde_json::json!({"a": [1]}), 2));
        assert!(json_depth_exceeds(&serde_json::json!({"a": [1]}), 1));
    }
}
