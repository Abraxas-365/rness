# Lua JSON reference

`rness.json` converts between Lua values and JSON text. The same Lua → JSON conversion is used wherever a plugin hands a table to rness: tool return values, interception hook returns, statusline and tool card views, command results, `rness.*` configuration tables.

Implementation: [`crates/rness-lua/src/runtime.rs`](../../../crates/rness-lua/src/runtime.rs) (`rness.json`), [`crates/rness-lua/src/json_guard.rs`](../../../crates/rness-lua/src/json_guard.rs) (limits).

## `rness.json.encode(value)`

```lua
local text = rness.json.encode({ name = "x", items = { 1, 2, 3 } })
```

Returns `value` as a JSON string.

- Tables with a sequence part become arrays, others objects. Functions, threads and userdata raise an error (except `rness.json.null`).
- Errors (ordinary Lua errors, catchable with `pcall`):
  - `table nesting exceeds 128 levels`: a table nested deeper than 128 levels (the outermost table is level 1).
  - `recursive table detected`: a table that contains itself, directly or indirectly.
  - `table too large to convert to JSON`: more than 1,000,000 nested tables (shared subtables count once per reference).

## `rness.json.decode(text)`

```lua
local value = rness.json.decode('{"a":[1,2]}')
```

Parses `text` and returns the Lua value. Invalid JSON, and JSON nested deeper than 128 levels, raise a Lua error.

## Limits everywhere else

The 128-level / no-cycle rule applies to every table rness converts:

| Where | What happens with a too-deep or cyclic table |
| --- | --- |
| Tool `run` return value | The tool call fails; the model sees the error message |
| Interception hook return (`pre_step`, `pre_tool`, …) | The hook chain fails with the error, like any hook error |
| Statusline, tool card, app view | The view is rejected; rness falls back to the default |
| `rness.*` configuration (init.lua, `rness.web`, …) | Loading the config fails with the error |

Data passed *to* Lua (hook payloads, tool arguments) is at most 128 levels deep when it comes from JSON; values deeper than 512 levels are refused.
