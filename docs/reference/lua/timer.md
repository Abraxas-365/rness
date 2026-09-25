# Lua timers and session delivery reference

`rness.timer` runs Lua callbacks later, without a user turn. `rness.session.send` and `rness.session.steer` let such a callback deliver text into a session. Together they are the building blocks for the [scheduled reminders plugin](../../guides/scheduled-reminders.md).

Implementation: [`crates/rness-lua/src/api/timer.rs`](../../../crates/rness-lua/src/api/timer.rs), [`crates/rness-lua/src/api/session.rs`](../../../crates/rness-lua/src/api/session.rs).

## Availability

| API | Available from |
| --- | --- |
| `rness.timer.*` | Everywhere, including the top level of startup `init.lua` |
| `rness.session.send` / `steer` | After engine mount (plugins, hooks, commands, timer callbacks). Not at the top level of `init.lua` |

## `rness.timer.after(seconds, fn)`

```lua
local id = rness.timer.after(30, function()
  rness.log.info("30 seconds later")
end)
```

Runs `fn` once, `seconds` after the call, and returns a numeric timer id.

- `seconds`: number, `> 0` and at most one year (31,622,400). Fractions are allowed.
- `fn`: function taking no arguments. Its return value is ignored.
- Errors: a Lua error when `seconds` is out of range or not finite.

## `rness.timer.every(seconds, fn)`

```lua
local tick = rness.timer.every(60, function() --[[ once a minute ]] end)
```

Runs `fn` repeatedly every `seconds` and returns a timer id.

- `seconds`: number, at least `1` and at most one year.
- The first run happens `seconds` after the call, not immediately.
- **No bursts.** A recurring timer that falls behind skips the missed slots instead of catching up. While the Lua host is busy, each timer has at most one pending run.
- An error in `fn` is logged (target `lua`, `timer <id> failed: ...`). The timer keeps running.

## `rness.timer.cancel(id)`

```lua
rness.timer.cancel(tick) --> true if it was active, false otherwise
```

Stops a timer. This is idempotent: cancelling an unknown, finished or already cancelled id returns `false`. A cancelled timer never runs again, even if it was already due.

## Execution model

- Deadlines are tracked on a background `lua-timer` thread. Callbacks always run on the Lua host thread, between other plugin work. They never run concurrently with hooks, tools or other callbacks.
- A slow callback delays all other Lua work, so keep callbacks short. One that runs longer than 30 seconds of Lua execution is aborted with `timer callback timed out`. The limit counts Lua instructions. It does not bound blocking native calls such as `rness.process`, or coroutines started inside the callback.
- Callbacks are synchronous and cannot yield. APIs that need to yield, such as `rness.llm.complete`, fail inside a timer callback. Use `rness.session.send` or `steer` to hand work to a model turn instead.
- Timers are process-local and in memory only. They do not survive a restart. In headless `-p` mode they stop when the process exits. Persist your own state (for example, a JSON file) if work must survive restarts.

## Ownership and lifecycle

- A timer created while a plugin loads, or inside any of that plugin's callbacks (hooks, tools, other timers), belongs to that plugin.
- Unloading the plugin, a failed load, or a hot reload cancels all of its timers. After a successful reload, the new code's timers replace the old ones; nothing is duplicated.
- Timers created at the top level of `init.lua` have no plugin owner. They live until the process exits.
- Module-level `local` state is recreated on hot reload. Keep state that must survive a reload in a global, for example `_G.__my_plugin_state = _G.__my_plugin_state or {}`.

## `rness.session.send(id, text)`

Delivers `text` as a user message with **queue** semantics (the TUI's Enter):

| Session state | Effect | Returns |
| --- | --- | --- |
| Idle | Starts a turn with `text` | `"started"` |
| Turn running | Queued; runs as its own turn after the current one ends | `"queued"` |
| Text is a slash command | Handled as that command | `"command"` |

## `rness.session.steer(id, text)`

Delivers `text` with **steer** semantics (the TUI's Ctrl+Enter):

| Session state | Effect | Returns |
| --- | --- | --- |
| Idle | Starts a turn with `text`, like `send` | `"started"` |
| Turn running | Joins that turn at its next model-step boundary, committed as `UserMessage{intent: Steer}`. In-flight requests and tools are not aborted | `"queued"` |
| Text is a slash command | Handled as that command | `"command"` |

A steer that arrives during a turn's final step runs as its own turn instead.

## Errors from `send` and `steer`

Both raise a Lua error when delivery is refused. The common case is a session that is temporarily busy, which includes compaction, a running command, or a plugin reload or unload:

```text
session is busy — wait for the active operation to finish and retry
```

This is transient, so retry later rather than giving up. Wrap calls in `pcall` inside timer callbacks:

```lua
local ok, err = pcall(rness.session.send, sid, "check the deploy")
if not ok and tostring(err):find("session is busy", 1, true) then
  -- try again on the next tick
end
```

If a turn was cancelled or failed, pending queued and steered messages stay parked. They are not run automatically. See [Queue and Steer](../../guides/queue-and-steer.md).

## Example: nudge a session every 10 minutes

Plugin code (for example `~/.rness/plugins/nudge.lua`):

```lua
local target -- session id, set when a turn starts here
rness.hook.on("turn_start", function(ev) target = ev.session end)

rness.timer.every(600, function()
  if not target or rness.session.phase(target) == "running" then return end
  pcall(rness.session.send, target, "Check whether the deploy finished.")
end)
```

This example skips delivery while a turn runs, so reminders never pile up. For durable, model-managed reminders, use the [schedule plugin](../../guides/scheduled-reminders.md) instead of writing your own.

## Related

- [Scheduled reminders guide](../../guides/scheduled-reminders.md)
- [Queue and Steer](../../guides/queue-and-steer.md)
- [Plugin loading and lifecycle](../../guides/plugins/loading-and-lifecycle.md)
