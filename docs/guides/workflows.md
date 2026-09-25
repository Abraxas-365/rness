# Workflows: fan work out across many subagents

A workflow is a short Lua script that the model writes and rness runs. The script starts subagents ("members"), runs them in parallel or in pipelines, and combines their results. Only the script's return value goes back to the conversation. The members' transcripts stay in their own child sessions.

Workflows are an **opt-in recipe**, not part of the default flavor. This guide covers how to turn them on, how a script works, how to write good ones, and the shipped [recipe scripts](../../examples/workflows/) you can copy. Exact field and limit details are in the [workflow tool reference](../reference/tools/workflow.md).

- [Enable workflows](#enable-workflows)
- [When a workflow fits](#when-a-workflow-fits)
- [How a script works](#how-a-script-works)
- [Recipes](#recipes)
- [Writing your own](#writing-your-own)
- [Run it](#run-it)
- [Troubleshooting](#troubleshooting)

## Enable workflows

You need three pieces in `~/.rness/init.lua`: the tool, at least one delegatable role, and (optionally) the live card.

**1. Turn the tool on.** Setting `rness.workflow` registers the `workflow` tool. An empty table uses the default limits:

```lua
rness.workflow = {}
-- or tune it:
rness.workflow = { max_concurrent_agents = 4, max_total_agents = 50 }
```

When `rness.workflow` is unset (the default), the tool does not exist and the model never sees it. See [limits](../reference/tools/workflow.md#execution-and-limits) for every key.

**2. Make delegation possible.** Members are ordinary subagents, so either declare roles with `subagent = true` or allow generic children:

```lua
rness.agents.declare("scout", {
  description = "Read-only exploration; returns file:line evidence.",
  instructions = "Explore and report concisely with file:line references.",
  subagent = true, tools = { "Glob", "Grep", "Read" },
})
-- or: rness.agents.allow_generic = true
```

The default flavor already declares `scout` and `reviewer` in [`lua/agents.lua`](../../flavors/default/lua/agents.lua). [`examples/lua/roles.lua`](../../examples/lua/roles.lua) has more, including a `worker` that can edit files. When generic children are disabled, every `agent()` call must name a role, and the tool description lists the enabled roles for the model. See [agent configuration](../reference/configuration/agents.md).

**3. (Optional) Add the live card.** Without a renderer, a workflow call shows the plain built-in tool card and the final result only. Copy [`examples/plugins/workflow-card.lua`](../../examples/plugins/workflow-card.lua) into your `plugins/` directory and add it to your single setup list:

```lua
rness.plugins.setup({
  -- … your other plugins …
  { name = "workflow-card", file = "plugins/workflow-card.lua" },
})
```

The card fits in 8 lines by default. To show more member rows, raise `preview_lines` for the tool:

```lua
rness.ui.messagebox = { tools = { workflow = { display = "preview", preview_lines = 14 } } }
```

Keep [tool exposure](plugins/example-recipes.md#deferred-tools-and-programmatic-tool-calling) at `native` (the default) or `both`. `ptc` mode hides the tool. Restart rness after changing `init.lua`.

## When a workflow fits

A workflow suits work made of **many independent pieces with a mechanical merge**:

| Task | Shape | Recipe |
| --- | --- | --- |
| Audit many files or crates | `pipeline(targets, scan, verify)` with schemas | [audit.lua](../../examples/workflows/audit.lua) |
| Review from several angles | `parallel({…})`, then one merging `agent()` | [review.lua](../../examples/workflows/review.lua) |
| Adversarial verification | find → `pipeline(candidates, disprove)` → keep survivors | [adversarial.lua](../../examples/workflows/adversarial.lua) |
| Mechanical migration | `pipeline(files, edit, check)`, then one build | [migrate.lua](../../examples/workflows/migrate.lua) |

It is the wrong tool for:

- **One or two delegations.** The plain [`subagent`](../reference/tools/subagent.md) tool is simpler and cheaper.
- **Exploratory work** where the next step depends on reading the last result. The script is fixed once it starts.
- **Anything needing the parent's judgment between steps.** Members run without the parent. Only the final value comes back.

Each member is a full model conversation. A 40-item audit with a verify stage is 80 model runs, so the cost scales accordingly.

## How a script works

The script is Lua 5.4. It gets a few globals, and it must end with `return <value>`:

| Global | What it does |
| --- | --- |
| `agent(prompt, opts?)` | Run one member to completion. Returns its final text, or a table when `opts.schema` is set. Returns `nil` if the member failed. |
| `parallel(thunks)` | Run zero-argument functions concurrently. Waits for all of them and returns their results in order. |
| `pipeline(items, stage1, stage2, …)` | Push each item through the stages independently, with no barrier between stages. Returns final values in item order. |
| `phase(title)` | Group later members under a heading on the card |
| `log(message)` | Add a progress line to the card |
| `compact(list)` | Drop `nil` holes, keeping order |
| `json.encode` / `json.decode` | Convert between Lua values and JSON text |
| `args` | The `args` object from the tool call |

`agent` options: `label` (card name), `phase`, `role` (a configured role), `provider` (`"spawn"`, the default, for a fresh context; `"fork"` to inherit the caller's history), and `schema`.

### Concurrency is implicit

Each `agent()` call **blocks the code that called it** until that member finishes. Concurrency comes only from `parallel` and `pipeline`:

```lua
-- Sequential: b starts after a finishes.
local a = agent("first")
local b = agent("second")

-- Concurrent: both start at once, and the call returns when both settle.
local r = parallel({
  function() return agent("first") end,
  function() return agent("second") end,
})
```

`pipeline(items, s1, s2)` runs `s1` then `s2` for each item, and items move independently: item 1 can be in `s2` while item 5 is still in `s1`. Stages are called as `stage(prev, item, index)`. For stage 1, `prev` is the item itself. `rness.workflow.max_concurrent_agents` caps how many members run at once. The rest queue.

### Schemas turn text into data

Without `schema`, `agent()` returns the member's final message as a string. With a schema, the member gets a `structured_output` tool and must report a value that validates against the schema. `agent()` then returns that value as a Lua table:

```lua
local report = {
  type = "object",
  required = { "findings" },
  properties = {
    findings = { type = "array", items = {
      type = "object",
      required = { "file", "line" },
      properties = { file = { type = "string" }, line = { type = "integer" } },
    } },
  },
}
local r = agent("List panics in src/", { schema = report })
if r then log(#r.findings .. " findings") end
```

The root must be an object. Supported keywords: `type`, `properties`, `required`, `additionalProperties`, `items`, `enum`, `const`, `oneOf`, `description`. Use a schema whenever the script needs to branch on, count, or merge results.

### Two kinds of failure

This split is the most important rule for writing scripts that hold up:

- **Ordinary failures become `nil`.** A member that errors, is cancelled, or never reports its structured result makes `agent()` return `nil`. An `error()` inside a pipeline stage or parallel thunk turns that item into `nil`. A stage returning `nil` skips the remaining stages for that item. The run carries on.
- **Misuse ends the whole run.** Examples: an unknown `agent` option, a bad schema, an unknown role, a tripped cap (agent count, items per call, CPU budget, memory), or an `error()` at the top level of the script. Running members are cancelled and the tool returns an error. `pcall` cannot catch this.

So always handle `nil`:

```lua
local results = pipeline(files, scan)
for i, file in ipairs(files) do          -- iterate the INPUT, not results:
  local r = results[i]                   -- ipairs(results) stops at the first nil
  if r then … else failed[#failed + 1] = file end
end
```

`compact(results)` works when you don't need to know *which* items failed.

### What the script can't do

The script VM has no `io`, `os`, `require`, `load`, or network access. It can't read files or run commands. **Members** do all the work with their own tools. The script's own Lua time is capped (`script_budget_ms`, default 5 s total). Time spent waiting on members doesn't count against it, so keep heavy processing out of the script.

## Recipes

[`examples/workflows/`](../../examples/workflows/) has four complete scripts. Each one starts with comments that explain its shape, plus `-- meta:` and `-- args:` lines giving a sample input. A test runs every recipe through the real scheduler, with and without member failures, so the recipes stay valid.

| Recipe | Pattern it shows |
| --- | --- |
| [audit.lua](../../examples/workflows/audit.lua) | Two-stage `pipeline` with schemas. The verify stage is skipped when the scan found nothing. Reports failed targets instead of dropping them. |
| [review.lua](../../examples/workflows/review.lua) | `parallel` fan-out, a barrier, then a merge. A failed angle is shown as "(this review failed)". Falls back to the raw reviews if the merge fails. |
| [adversarial.lua](../../examples/workflows/adversarial.lua) | Dependent stages. A top-level `error()` ends the run when there is nothing to verify, and an unverified claim is treated as not holding. |
| [migrate.lua](../../examples/workflows/migrate.lua) | Edit-then-check per file, with `nil` returned to skip the check. One build after the pipeline barrier. Needs a role that can edit. |

Each recipe names its roles in a line near the top (`local SCAN_ROLE, VERIFY_ROLE = "scout", "reviewer"`). Change them to your roles, or set them to `nil` when `allow_generic = true`.

There are two ways to use a recipe:

1. **Point the model at it.** "Use the workflow tool with `examples/workflows/audit.lua` as the script, targets = every crate under `crates/`." The model reads the file and passes its contents as `script` and your values as `args`.
2. **Describe the shape.** "Use the workflow tool: one scout per crate lists `unwrap()` in non-test code with a `{file, line, why}` schema, then a reviewer verifies each crate's findings. Return only confirmed findings." The model writes an equivalent script.

## Writing your own

Start from the recipe closest to your shape. Then work through this checklist:

1. **Make items independent.** Each member should do its whole piece without needing another member's output. If members edit files, give each one a disjoint set of files. Concurrent edits to one file will conflict.
2. **Write self-contained prompts.** A `spawn` member knows only its prompt. Include the scope, the goal, and what "done" means. Use `provider = "fork"` only when a member truly needs the conversation so far. Forks cost more.
3. **Use a schema when the script inspects the result.** Keep schemas small and put `required` on the fields you read.
4. **Handle every `nil`.** Iterate the input list with an index, and report failures instead of dropping them silently.
5. **Label members and use phases.** `label = target` and `phase("Verify")` make the live card readable, and they show you which child session to open.
6. **Keep the return value small.** Return findings, not transcripts. Results over `max_result_chars` (default 50,000) are truncated.
7. **Bound the fan-out.** Check the item count before a large pipeline. `max_total_agents` and `max_items_per_call` end the run when exceeded. They don't clip it.
8. **Test on a small `args` first.** Run with two targets before running with two hundred.

A minimal skeleton:

```lua
-- meta: {"name":"my-workflow","description":"One line for the card"}
-- args: {"items":["a","b"]}
local ROLE = "scout"
local shape = { type = "object", required = { "answer" }, properties = { answer = { type = "string" } } }

phase("Work")
local results = pipeline(args.items, function(item)
  return agent("Do X for " .. item .. ". Report the answer.", { label = item, role = ROLE, schema = shape })
end)

local out, failed = {}, {}
for i, item in ipairs(args.items) do
  if results[i] then out[#out + 1] = { item = item, answer = results[i].answer }
  else failed[#failed + 1] = item end
end
return { results = out, failed = failed }
```

## Run it

**Ask for it by name.** The model should use `workflow` only when you ask for a workflow or the task clearly needs large fan-out:

> Use the workflow tool to audit every crate under `crates/` for `unwrap()` in non-test code …

**Follow the run.** With the card plugin enabled, the call renders a live card that updates about five times a second:

```text
workflow: unwrap-audit · running · 42s
Audit crates for unwrap in non-test code
Phase: Verify
Agents: 14 · 3 running · 2 queued · 8 done · 1 failed · 0 cancelled
  ● rness-engine verify · Verify
  ✓ rness-engine · Scan
  …
› rness-tools: 3 of 5 confirmed
```

Each member is a child session. Select one in the agent monitor to read its transcript. When the run ends, the card shows the final counts and the first lines of the result.

**Stop it.** Interrupting the parent turn (for example with Ctrl+C) cancels the script and every running member. The model receives an error result saying the workflow was cancelled.

## Troubleshooting

| What you see | Meaning and fix |
| --- | --- |
| The model never uses the tool | `rness.workflow` is unset, or exposure is `ptc`. Check `init.lua` and restart. |
| `Workflows are unavailable…` | No delegation is possible. Declare a role with `subagent = true` or set `allow_generic = true`. |
| `… agent is not enabled for delegation: x` | A recipe's role names don't match your config. Edit the `ROLE` line. |
| `… generic subagents are disabled …` | Generic children are disabled, so every `agent()` must pass `role`. |
| Some items are `null` in the result | Those members failed, were cancelled, or never reported their structured result. Open them in the agent monitor. |
| `failed after starting N agents: …` for anything else | Misuse or a top-level `error()`. The message names the cause. Running members were cancelled. |
| `… exceeded its CPU budget …` | The script's own Lua ran over `script_budget_ms`. Move work into members, or raise the budget. |
| Result ends with `[workflow result truncated …]` | Return a summary, or raise `max_result_chars`. |
| Plain card with no progress | The `workflow-card` plugin isn't in your setup list. |

Runs are foreground only: the parent turn waits. They are not resumed after a restart. Members' child sessions survive, but the run itself does not. See [known limitations](../project/known-limitations.md).
