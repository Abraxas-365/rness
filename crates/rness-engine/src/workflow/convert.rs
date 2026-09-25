//! Lua ↔ JSON at the workflow boundary (args in, child results in, the
//! script's return value out).
//!
//! Lua has one table type for arrays and objects, so shape is tracked in a
//! weak-keyed side table: tables the engine builds from JSON arrays (and the
//! results of `parallel`/`pipeline`/`compact`) are registered with their
//! length, so `nil` holes — failed items — survive as JSON `null`, and empty
//! JSON objects are registered so they round-trip as `{}`. Unregistered
//! tables are classified by their keys; an empty unregistered table is `[]`.

use std::collections::HashSet;
use std::ffi::c_void;

use mlua::{Lua, Table, Value as LuaValue};
use serde_json::{Map, Number, Value};

const SHAPES: &str = "rness.workflow.shapes";
const MAX_DEPTH: usize = 128;

pub(crate) fn install(lua: &Lua) -> mlua::Result<()> {
    let shapes = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__mode", "k")?;
    shapes.set_metatable(Some(meta));
    lua.set_named_registry_value(SHAPES, shapes)
}

fn shapes(lua: &Lua) -> mlua::Result<Table> {
    lua.named_registry_value(SHAPES)
}

/// A registered array: `nil` entries stay holes and serialize as `null`.
pub(crate) fn array(lua: &Lua, values: Vec<LuaValue>) -> mlua::Result<Table> {
    let table = lua.create_table_with_capacity(values.len(), 0)?;
    let len = values.len();
    for (i, value) in values.into_iter().enumerate() {
        if !value.is_nil() {
            table.raw_set(i + 1, value)?;
        }
    }
    shapes(lua)?.raw_set(table.clone(), len)?;
    Ok(table)
}

/// Sequence length honoring registration: `max(registered, border)`.
pub(crate) fn seq_len(lua: &Lua, table: &Table) -> mlua::Result<usize> {
    let registered = match shapes(lua)?.raw_get::<LuaValue>(table.clone())? {
        LuaValue::Integer(n) => n.max(0) as usize,
        _ => 0,
    };
    Ok(registered.max(table.raw_len()))
}

pub(crate) fn to_lua(lua: &Lua, value: &Value) -> mlua::Result<LuaValue> {
    Ok(match value {
        Value::Null => LuaValue::Nil,
        Value::Bool(b) => LuaValue::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => LuaValue::Integer(i),
            None => LuaValue::Number(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => LuaValue::String(lua.create_string(s)?),
        Value::Array(items) => {
            let items = items
                .iter()
                .map(|v| to_lua(lua, v))
                .collect::<mlua::Result<Vec<_>>>()?;
            LuaValue::Table(array(lua, items)?)
        }
        Value::Object(fields) => {
            let table = lua.create_table()?;
            for (key, value) in fields {
                let value = to_lua(lua, value)?;
                if !value.is_nil() {
                    table.raw_set(key.as_str(), value)?;
                }
            }
            if fields.is_empty() {
                shapes(lua)?.raw_set(table.clone(), false)?;
            }
            LuaValue::Table(table)
        }
    })
}

/// Materialize a Lua value as plain JSON data. The error names the
/// offending path (`value.issues[3]`) and what was wrong there. `max_bytes`
/// bounds the materialized size (an estimate: string bytes plus a fixed
/// per-node cost), so shared subtrees — `t = {t, t}` repeated — cannot
/// expand exponentially outside the VM's memory limit.
pub(crate) fn from_lua(
    lua: &Lua,
    value: &LuaValue,
    root: &str,
    max_bytes: usize,
) -> Result<Value, String> {
    let shapes = shapes(lua).map_err(|e| e.to_string())?;
    let mut ctx = Ctx {
        shapes,
        open: HashSet::new(),
        left: max_bytes,
        max: max_bytes,
    };
    ctx.convert(value, root, 0)
}

/// Estimated bytes per converted node (a `serde_json::Value` plus overhead).
const NODE_COST: usize = 32;

struct Ctx {
    shapes: Table,
    open: HashSet<*const c_void>,
    left: usize,
    max: usize,
}

enum Shape {
    Array(usize),
    Object,
    Unknown,
}

impl Ctx {
    fn charge(&mut self, bytes: usize, path: &str) -> Result<(), String> {
        match self.left.checked_sub(NODE_COST.saturating_add(bytes)) {
            Some(left) => {
                self.left = left;
                Ok(())
            }
            None => Err(format!(
                "{path}: value is too large to convert (over {} MiB as JSON data)",
                (self.max / (1024 * 1024)).max(1)
            )),
        }
    }

    fn convert(&mut self, value: &LuaValue, path: &str, depth: usize) -> Result<Value, String> {
        let bytes = match value {
            LuaValue::String(s) => s.as_bytes().len(),
            _ => 0,
        };
        self.charge(bytes, path)?;
        Ok(match value {
            LuaValue::Nil => Value::Null,
            LuaValue::LightUserData(ud) if ud.0.is_null() => Value::Null,
            LuaValue::Boolean(b) => Value::Bool(*b),
            LuaValue::Integer(i) => Value::from(*i),
            LuaValue::Number(f) => Number::from_f64(*f)
                .map(Value::Number)
                .ok_or_else(|| format!("{path}: non-finite number {f}"))?,
            LuaValue::String(s) => Value::String(
                s.to_str()
                    .map_err(|_| format!("{path}: string is not valid UTF-8"))?
                    .to_owned(),
            ),
            LuaValue::Table(table) => self.table(table, path, depth)?,
            other => {
                return Err(format!(
                    "{path}: {} values are not JSON data",
                    other.type_name()
                ))
            }
        })
    }

    fn table(&mut self, table: &Table, path: &str, depth: usize) -> Result<Value, String> {
        if depth >= MAX_DEPTH {
            return Err(format!("{path}: nested deeper than {MAX_DEPTH} levels"));
        }
        let ptr = table.to_pointer();
        if !self.open.insert(ptr) {
            return Err(format!("{path}: table contains itself (cycle)"));
        }
        let result = self.table_body(table, path, depth);
        self.open.remove(&ptr);
        result
    }

    fn table_body(&mut self, table: &Table, path: &str, depth: usize) -> Result<Value, String> {
        let shape = match self.shapes.raw_get::<LuaValue>(table.clone()) {
            Ok(LuaValue::Integer(n)) => Shape::Array(n.max(0) as usize),
            Ok(LuaValue::Boolean(false)) => Shape::Object,
            _ => Shape::Unknown,
        };
        let mut indexed: Vec<(usize, LuaValue)> = Vec::new();
        let mut named: Vec<(String, LuaValue)> = Vec::new();
        for pair in table.clone().pairs::<LuaValue, LuaValue>() {
            let (key, value) = pair.map_err(|e| format!("{path}: {e}"))?;
            match key {
                LuaValue::Integer(i) if i >= 1 => indexed.push((i as usize, value)),
                LuaValue::String(s) => named.push((
                    s.to_str()
                        .map_err(|_| format!("{path}: key is not valid UTF-8"))?
                        .to_owned(),
                    value,
                )),
                other => {
                    return Err(format!(
                        "{path}: table keys must be strings or positive integers (found {})",
                        other.type_name()
                    ))
                }
            }
        }
        if !indexed.is_empty() && !named.is_empty() {
            return Err(format!("{path}: table mixes array items and named fields"));
        }
        let as_array = match shape {
            Shape::Array(_) => true,
            Shape::Object => !indexed.is_empty(),
            Shape::Unknown => named.is_empty(),
        };
        if !as_array {
            if !indexed.is_empty() {
                return Err(format!("{path}: table mixes array items and named fields"));
            }
            named.sort_by(|a, b| a.0.cmp(&b.0));
            let mut object = Map::new();
            for (key, value) in named {
                let child = format!("{path}.{key}");
                self.charge(key.len(), &child)?;
                object.insert(key, self.convert(&value, &child, depth + 1)?);
            }
            return Ok(Value::Object(object));
        }
        if !named.is_empty() {
            return Err(format!("{path}: table mixes array items and named fields"));
        }
        let registered = match shape {
            Shape::Array(n) => n,
            _ => 0,
        };
        let max = indexed.iter().map(|(i, _)| *i).max().unwrap_or(0);
        let len = registered.max(max);
        if max > registered && max > 64 && max > indexed.len() * 2 {
            return Err(format!(
                "{path}: sparse array (index {max} with only {} items)",
                indexed.len()
            ));
        }
        // Holes are materialized as `null` too.
        self.charge(
            len.saturating_sub(indexed.len()).saturating_mul(NODE_COST),
            path,
        )?;
        let mut items = vec![Value::Null; len];
        for (i, value) in indexed {
            items[i - 1] = self.convert(&value, &format!("{path}[{i}]"), depth + 1)?;
        }
        Ok(Value::Array(items))
    }
}

/// Lua writes an empty table for `properties = {}`; the neutral `[]`
/// reading would make a valid schema fail the subset check.
pub(crate) fn normalize_schema(schema: &mut Value) {
    let Value::Object(fields) = schema else {
        return;
    };
    if matches!(fields.get("properties"), Some(Value::Array(a)) if a.is_empty()) {
        fields.insert("properties".into(), Value::Object(Map::new()));
    }
    if let Some(Value::Object(props)) = fields.get_mut("properties") {
        props.values_mut().for_each(normalize_schema);
    }
    if let Some(items) = fields.get_mut("items") {
        normalize_schema(items);
    }
    if let Some(Value::Array(branches)) = fields.get_mut("oneOf") {
        branches.iter_mut().for_each(normalize_schema);
    }
}
