# Workflow tool reference

`workflow` runs a Lua script written by the model. The script coordinates subagents, fanning work out across many independent pieces: audits over many files, migrations, research from several angles, or adversarial checks of findings. The model writes the coordination once as a script and doesn't have to delegate turn by turn. Only the script's return value reaches the calling conversation. Member transcripts do not.

This tool matches the `workflow` package in deepseek-harness. The rness differences are listed [below](#differences-from-deepseek-harness).

## Availability

- The tool is opt-in. It is registered at startup only when `init.lua` sets `rness.workflow` (`rness.workflow = {}` uses the default [limits](#execution-and-limits)). The default flavor does not set it. See the [workflows guide](../../guides/workflows.md#enable-workflows) for the full setup.
- It needs session-aware dispatch. Calling it without a session fails.
- When generic children are disabled and no role is enabled for delegation, the description is replaced with a refusal and every call fails with `Workflows are unavailable…`.
- When generic children are disabled, the description tells the model that each `agent()` call must set `opts.role`. Enabled roles are listed with their descriptions.
- `ptc` [tool exposure](../../guides/plugins/example-recipes.md#deferred-tools-and-programmatic-tool-calling) omits `workflow`. It is neither listed nor returned by `ToolSearch`. In `native` and `both` modes it is an ordinary tool, and it can be deferred.
- `run_code` programs cannot call `workflow`. Its members (the child sessions it starts) cannot see or call it either, so workflows never nest.
- The default flavor's `tool:workflow` [system-prompt section](../configuration/agents.md#sections) tells the model to use workflows only when explicitly asked for one or for large multi-agent orchestration. It is sent only while the tool is usable, the same usage policy dsh ships.

## Input

| Field | Type | Required | Behavior |
| --- | --- | --- | --- |
| `meta` | object | Yes | Workflow identity. This is data and is never evaluated. |
| `meta.name` | string | Yes | Short kebab-case name, such as `panic-audit` |
| `meta.description` | string | Yes | One line, shown on the card |
| `meta.whenToUse` | string | No | Free text |
| `meta.phases` | array of `{title, detail?}` | No | Planned phases |
| `script` | string | Yes | Lua 5.4 chunk ending in `return <value>` |
| `args` | object | No | Available to the script as the global `args` |

Unknown top-level fields are rejected. `meta` and `args` may also be sent as JSON text; some models do this. Before any member starts, the arguments are validated and the script is checked for size and syntax. Failures at this stage are ordinary error results and never start a run.

## Script API

| Global | Behavior |
| --- | --- |
| `agent(prompt, opts?)` | Run one member to completion and return its final assistant text. |
| `pipeline(items, stage1, …)` | Process each item through the stages independently, with no barrier between stages. Each stage is called as `stage(prev, item, index)`. Stage 1 receives `prev = item`. Returns the final values in item order. |
| `parallel(thunks)` | Call zero-argument functions concurrently and wait for all of them. |
| `compact(list)` | Remove `nil` holes and keep the order. |
| `json.encode(v)` / `json.decode(s)` | Convert between Lua values and JSON. |
| `phase(title)` | Start a progress phase. Later members are grouped under it. |
| `log(message)` | Add a progress line to the card. |
| `args` | The `args` input, or an empty table |

`agent` options:

| Option | Behavior |
| --- | --- |
| `label` | Display name on the card |
| `phase` | Progress group, overriding the current `phase()` |
| `role` | A configured role, as in the `subagent` tool's `agent` field |
| `provider` | `"spawn"` (default): fresh context. `"fork"`: inherits the caller's completed history. |
| `schema` | JSON Schema with an object root. The member must report a result that validates against it through its `structured_output` tool, and `agent` returns that table instead of text. |

Schemas may use only `type`, `properties`, `required`, `additionalProperties`, `items`, `enum`, `const`, `oneOf`, and `description`.

The return value must be JSON-serializable. A table with keys `1..n` becomes an array. A table with string keys becomes an object. An empty table becomes `[]`, unless it came from a JSON object, which round-trips as `{}`. `nil` holes in `pipeline`/`parallel`/`compact` results and in JSON-derived arrays are kept as `null`.

## Failure semantics

An ordinary failure affects one item:

- `agent` returns `nil` when a member fails, is cancelled, or ends without reporting its structured result.
- An ordinary `error()` inside a pipeline stage or a parallel thunk turns that item's result into `nil`. Its remaining stages are skipped. A stage that returns `nil` also skips the rest.

Misuse ends the whole run. Examples: bad arguments to a global, unknown or deferred `agent` options, an unsupported schema, an unknown role, or a tripped cap (agent count, items per call, CPU budget, memory). The Rust scheduler makes this decision. `pcall` cannot catch it, and it never turns into a per-item `nil`. If the runtime can't create a member (a store or session error), only that item fails: `agent` returns `nil`.

The run itself ends in one of three states:

- `completed`: the result is `workflow "<name>" completed (N agents).`, followed by the value as pretty-printed JSON.
- `error`: an error result, `workflow "<name>" failed after starting N agents: <reason>`.
- `cancelled`: interrupting the parent turn cancels the run and every running member. The result is an error with the text `was cancelled`.

## Execution and limits

The tool runs in the foreground: the call returns when the script finishes. Each member is a one-shot child session of the calling session, created through the same runtime as `subagent`. Members inherit the caller's tool ceiling, minus `workflow`. They can be opened from the agent monitor like any other child.

Configure limits at startup in `init.lua`. Setting the table enables the tool. Every key is optional, each value must be a positive integer, and unknown keys are rejected:

```lua
rness.workflow = {
  max_concurrent_agents = 8,   -- default: CPU cores - 2, clamped to 1..16
  max_total_agents = 1000,     -- members per run
  max_items_per_call = 4096,   -- items per pipeline/parallel call
  script_budget_ms = 5000,     -- total time spent running the script's own Lua; waiting on members is free
  memory_mb = 64,              -- script VM memory; also bounds converting a value to JSON
  max_script_bytes = 65536,    -- script source size
  dispose_grace_ms = 5000,     -- wait for members to settle after the script ends
  max_result_chars = 50000,    -- rendered result cap; longer values are truncated with a note
}
```

The script VM has no `io`, `os`, `require`, `load`, `collectgarbage`, or network access, and `setmetatable` rejects `__gc` metamethods. Lua runs finalizers with hooks disabled, so a finalizer could escape the budget and cancellation. For the same reason the shared string metatable is hidden (`getmetatable("")` returns `false`), and non-string error objects are converted to text inside the budgeted script, so a looping `__tostring` counts against the budget. Only members do work; the script only coordinates them.

The budget is enforced every 1000 VM instructions. A single library call can't be interrupted, for example `string.find` with a pathological pattern. If one runs more than 2 seconds past the budget, the run fails, its members are cancelled, and the script thread is abandoned in the background. Cancelling the parent turn behaves the same way when the script doesn't stop within the dispose grace period plus 2 seconds.

## Live card

While a run is active, rness re-renders its tool card about five times a second from a `presentation` snapshot with `kind = "workflow_activity"`. The snapshot holds the name, description, status, `elapsed_ms`, current phase, `total`, `counts` by status, up to 12 `members` (running first, each with `label`, `phase`, `status`, `session`), and the last six `logs` lines. When the run ends, the card is re-rendered from the stored tool result. That result's presentation metadata is the final snapshot, and `output` holds the result text. Because it is rebuilt from the stored result, the card reflects any `post_tool` hook or `tool_execute` wrapper that replaced the output.

No renderer ships in the default flavor. Without one, the call uses the built-in tool card and shows no live progress. [`examples/plugins/workflow-card.lua`](../../../examples/plugins/workflow-card.lua) is a complete renderer (`rness.ui.tool_card("workflow", …)`) that shows the snapshot plus up to eight lines of the result.

## Example

```json
{
  "meta": {"name": "panic-audit", "description": "Find panics in non-test code"},
  "args": {"crates": ["rness-engine", "rness-tools"]},
  "script": "local shape = { type = 'object', required = { 'issues' }, properties = { issues = { type = 'array', items = { type = 'string' } } } }\nphase('Scan')\nlocal reports = pipeline(args.crates, function(_, crate)\n  return agent('List panics in non-test code of crates/' .. crate, { label = crate, schema = shape })\nend)\nreturn compact(reports)"
}
```

## Differences from deepseek-harness

- Scripts are Lua 5.4, not JavaScript.
- Misuse is always fatal. deepseek-harness sometimes let `try/catch` swallow it.
- Results are capped by `max_result_chars`.
- `effort`, `isolation`, `agentType`, and `model` are recognized as deferred `agent()` options and rejected. Nested workflows are not supported.

Implementation: [tool](../../../crates/rness-tools/src/workflow.rs), [engine](../../../crates/rness-engine/src/workflow/), [card state](../../../crates/rness-engine/src/workflow/activity.rs).
