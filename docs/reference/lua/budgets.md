# Lua execution budgets

All plugins share one Lua VM on one thread. While a callback runs, every other Lua-driven feature waits: the statusline, apps, tool cards, hooks, slash commands. To keep one bad callback from freezing the rest, every entry into Lua runs under a **budget**. When the budget is exceeded, the callback is stopped with a Lua error.

| Entry point | Budget | Repeat offenders |
|---|---|---|
| Hook handler (`rness.hook.on` for notifications such as `frame`, `turn_start`) | 500 ms per handler | unsubscribed after 3 overruns in a row |
| Statusline provider (function or table form) | 250 ms | disabled after 3 overruns in a row |
| App `view` / `on_key` | 1 s | app disabled after 3 overruns in a row |
| Plugin action (key binding) | 1 s | — |
| Timer callback (`rness.timer`) | 1 s | timer cancelled after 3 overruns in a row |
| Tool card renderer | 100 ms (and 16 MiB extra memory) | — |
| Plugin chunk (load or hot reload) | 5 s | the reload fails and the previous plugin set stays; turns submitted while a reload runs are rejected as busy, so a runaway chunk blocks new turns for up to 5 s |
| `init.lua` at startup | 30 s | startup fails with an error |
| Interception hooks (`pre_step`, `pre_tool`, …) and web hooks | 30 s, or until the turn is cancelled | — |
| `tool_execute` hooks | the caller's deadline, or until cancelled | — |
| Slash commands, completions, Lua tools | no time limit; stopped by cancel (Ctrl+C) | — |
| Background task (`rness.task`) | about 30 million instructions between two awaits | — |

"In a row" means consecutive calls: one call that finishes in time resets the count. A disabled handler stays off until its plugin is reloaded. When one is disabled, rness shows a notice in the session, e.g. `Lua 'frame' hook handler exceeded its 500 ms budget 3 times in a row and was disabled until the plugin is reloaded`, and also writes it to the `lua` log target.

Nested calls use the tightest active budget. For example, `rness.events.emit` called from a hook runs its handlers inside that hook's 500 ms.

## Budgets cannot be caught

A budget error is a normal Lua error message, but once a budget has run out it cannot be swallowed. `pcall`, `xpcall`, `coroutine.wrap` and `coroutine.resume` see the error, and the next Lua instruction raises it again. That continues until the callback returns to rness. Code like this is still stopped:

```lua
while true do pcall(function() while true do end end) end
```

Cancellation (Ctrl+C on a command, a cancelled turn) works the same way.

## Blocking APIs inherit the budget

`rness.http.get/request` and `rness.process.run` block the VM while they wait. Inside a budgeted callback they are clamped to the time that is left. A `rness.process.run` in a 500 ms hook is killed when the hook's budget runs out, not after its own 30 s default. The process error reads `process exceeded the caller's time budget`.

For slow I/O, start a [background task](task.md) instead. Awaiting APIs such as `rness.llm.complete` let the VM keep serving everything else.

## Limits

- **Native calls are not interrupted.** Budgets are checked between Lua instructions. A single long call into C, such as a pathological `string.find` pattern over a large string, a big `string.rep`, or a slow Rust API like a first `rness.session.usage` over a very long log, runs to completion. The budget applies again as soon as the call returns, and the overrun counts as a strike.
- **Memory.** The VM heap is capped at 384 MiB. An allocation past that limit fails with a Lua `not enough memory` error instead of exhausting the machine.
- Budgets are fixed, not configurable. They are intentionally generous: they exist to un-wedge the VM, not to measure speed. Keep callbacks short. A statusline that is called many times a second should finish in microseconds.
- The statusline value is truncated to 4 KiB per string (64 entries per table, 4 levels deep). It is one terminal row.
