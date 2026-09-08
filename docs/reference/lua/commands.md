# Lua commands and session context

Load the plugin explicitly with `rness.plugins.load("commands")` from init.lua. Registration occurs during plugin loading, not inside callbacks.

```lua
rness.commands.register {
  name = "greet",
  description = "Display a greeting",
  usage = "<name>",
  arguments = { "world", "team" },
  run = function(ctx)
    return {
      message = "Hello" .. ctx.raw_input,
      data = { session = ctx.session, workspace = ctx.workspace },
    }
  end,
}
```

Use `/greet world`, `/help`, or `/help greet`. `arguments` supplies static suffix candidates for the command picker; it does not validate or interpolate input. Candidates update when plugins load or unload. Usage and descriptions generate help automatically. Runtime completion callbacks and shell-style argument parsing are not provided.

`ctx.session` identifies the invoking session. `ctx.workspace` is its saved workspace, or absent for legacy sessions without one. `ctx.raw_input` preserves the text after the command name, including separator whitespace.

A callback may return a string, nil, or `{ message = string, data = JSON-compatible value }`. Errors become submission failures. Results are local notices in the TUI and `{ "status": "command", "result": ... }` over HTTP. They are not appended as model conversation messages. TUI notices are ephemeral; a result belonging to another currently hidden session is not displayed in the active session.

Names must start with a lowercase letter and contain lowercase letters, digits, hyphens or underscores. Built-in names are reserved. Duplicate registrations fail. Unload removes owned handlers, metadata and completion entries without removing unrelated commands.

## Execution and cancellation

TUI commands and HTTP submissions dispatch through `SessionService::send_async`, keeping frontend/runtime threads free while the Lua actor runs the handler. Rust handlers execute on a blocking worker and receive a cancellation token. Rust integrations must poll or await that token themselves. The synchronous `send` entry point remains available for synchronous callers.

Only one command runs per session. Another command or model submission is rejected while it runs. A command is rejected during an active model turn. Extension maintenance cannot acquire its reservation while a command executes. Cancellation uses the ordinary session cancel operation; Lua instruction hooks interrupt Lua execution, including infinite Lua loops. The execution reservation remains held until the handler exits.

Cancellation is cooperative, not process isolation: a blocking native function or external side effect cannot be forcibly interrupted by a Lua instruction hook. Lua handlers share one actor and do not run concurrently in separate VMs. Do not use blocking native calls for unbounded operations, swallow cancellation errors, or recursively invoke Lua commands through the session bridge. Cancellation does not roll back side effects. Hot reload of registered commands requires restarting; coordinated unload is supported.

## Custom tool context

Lua tools receive a second argument when dispatched through a workspace-bound turn registry:

```lua
rness.tool.register {
  name = "project-path",
  description = "Return the active project path",
  schema = { type = "object" },
  run = function(args, ctx)
    return ctx.workspace or "No session workspace"
  end,
}
```

Use `ctx.workspace` to construct paths explicitly. The host never changes the process working directory. Direct calls outside a scoped turn receive an empty context unless the caller supplies one. Native Rust tools can implement `Tool::for_workspace(session, workspace)` to return a bound adapter. This is context propagation, not a sandbox or filesystem restriction.
