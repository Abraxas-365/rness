# Lua background tasks reference

`rness.task` runs Lua work in the background that can wait on async APIs such as `rness.llm.complete` and `rness.session.generate_title`. Hooks, timers and statusline renderers are synchronous and cannot yield. A task is how they start work that takes seconds without blocking other plugins. The default [session-title plugin](../../guides/configuration/tui-commands.md#session-titles) is built on it.

Implementation: [`crates/rness-lua/src/api/task.rs`](../../../crates/rness-lua/src/api/task.rs).

## `rness.task.spawn(fn, ...)`

```lua
rness.hook.on("prompt", function(ev)
  rness.task.spawn(function(session)
    local ok, title = pcall(rness.session.generate_title, session)
    if ok then rness.session.offer_title(session, title, "model") end
  end, ev.session)
end)
```

This queues `fn(...)` to run on the Lua host thread and returns a numeric task id. Spawning never runs `fn` inline, so it is safe from inside any callback. The extra arguments are passed to `fn` when it starts.

- `fn` may call yielding rness APIs. While one is in flight, the VM keeps serving hooks, tools, commands and other tasks. When the call completes, the task resumes with the result, or with a Lua error you can catch with `pcall`.
- Plain `coroutine.yield` is an error: tasks may only yield through async rness APIs.
- An error in `fn` is logged (target `lua`, `task failed: ...`). It never reaches the caller of `spawn`.
- Between two awaits, a task gets a Lua instruction budget of about 30 million instructions. Exceeding it aborts the task.
- At most 256 tasks can be live at once. Beyond that, `spawn` raises an error.
- `spawn` raises an error in a host without a task runner, such as a bare `LuaRuntime`. Awaiting also needs the engine to be mounted. In a normal `rness` process both are always true.

## `rness.task.cancel(id)`

```lua
rness.task.cancel(id) --> true if it was live, false otherwise
```

Stops a task. This is idempotent. An in-flight await is aborted: the model request is dropped, and the task never resumes. A task spinning in a loop is stopped at its next instruction check.

## `rness.task.running()`

Returns the number of live tasks.

## Ownership and lifecycle

- A task spawned while a plugin loads, or inside any of that plugin's callbacks (hooks, tools, timers, other tasks), belongs to that plugin.
- Unloading the plugin, a failed load, or a hot reload cancels its tasks, including those parked on an await. Registrations a task makes (hooks, timers, further tasks) belong to the same plugin.
- Tasks are process-local and in memory only. They do not survive a restart.

## Related

- [Lua timers](timer.md): synchronous, time-based callbacks. Spawn a task from a timer to do async work.
- [Plugin loading and lifecycle](../../guides/plugins/loading-and-lifecycle.md)
