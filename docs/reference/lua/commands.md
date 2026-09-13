# Lua commands and session context

## Opt-in structured questions

Enable the built-in question tool in `~/.rness/plugins/questions.lua`, loaded explicitly with a `{name='questions', file='./plugins/questions.lua', watch=true}` entry in `rness.plugins.setup({...})` from `init.lua`:

```lua
rness.questions.enable {
  enabled = true, -- false removes the tool and dismisses pending questions
  title = "AskUser",
  height = 20, -- minimum 10; actual drawing is clipped to the terminal
  priority = 99, -- slot priority, below approval by default
}
```

Calling `enable()` without arguments uses these defaults. No tool is registered unless explicitly enabled. `enable` and `disable` are plugin-load declarations, not callback APIs. The last successful enabling plugin owns the configuration; unloading that owner removes the tool and dismisses pending questions. For the explicitly named source above, use `/unload questions`. Unloading another plugin does not disable questions. Failed loads and failed reloads do not apply question changes. Successful reload replaces the owner/configuration from the new plugin set. Startup declarations in `init.lua` remain supported.

The stock renderer remains mounted but inactive without pending questions. It reads title, height and priority dynamically and uses the active theme. An explicitly opened Lua overlay (such as the session picker) temporarily takes focus; closing it restores the question. Drafts are preserved per session/call. Closing the local frontend dismisses pending questions and removes the tool. Headless prompt mode removes AskUser because it has no human answer frontend.

`AskUser` accepts `questions`, an array of 1–16 objects with unique `id`, `question`, optional `header`, `options` (`label`, optional `description`), and `multi_select`. Custom text is always allowed. Results contain `answers`, each with `id`, `selected` labels and optional `custom`. Invalid answers leave the question pending for correction; cancellation or dismissal removes it and returns a tool error. Headless execution without a question frontend fails immediately.

TUI: Up/Down moves through options, Space toggles a selection (single or multiple), Enter advances/submits. Select Other to edit custom text, Enter finishes editing, and Enter again advances/submits. Tab/Shift-Tab revisits questions without losing answers. Esc dismisses; Ctrl+C cancels the turn. Long option lists scroll with the selection. Labels wrap; oversized items show a preview. PgDown opens full question/option details and scrolls down, PgUp scrolls back, and Esc returns to selection. The editor follows the end of long text. Terminals smaller than 30×10 show a resize/cancel notice instead of an unusable form.

HTTP: `GET /api/questions` reconciles pending questions across sessions. `POST /api/questions/:session/:call` accepts `{ "answers": [{ "id": "q", "selected": ["A"], "custom": null }] }`; `DELETE` on the same URL dismisses. SSE `/api/events` and `/api/events/:session` emit `question_requested` (session, call, questions) and `question_resolved` (session, call). SSE is ephemeral; reconnecting clients must fetch pending questions. Resolution is first-writer-wins; disconnecting an SSE client does not dismiss questions, allowing reconnects and other clients to answer.

## Opt-in tool permissions

In `init.lua`, explicitly configure exact tool names:

```lua
rness.permissions.set {
  Read = "allow",
  Write = "ask",
  Bash = "deny",
}
```

No configuration means unrestricted tool execution by default. Rules are case-sensitive and apply to agent-dispatched Rust, Lua, and MCP tools regardless of their sensitivity flag. `allow` executes, `deny` blocks, and `ask` requires approval for each call. Missing or disconnected approvers fail closed. TUI and HTTP/SSE use their existing approval interfaces.

Explicit rules override the global `--approval` setting for that tool. Unmatched tools retain the existing global behavior: `allow` permits everything; `ask`/`never` affect sensitive tools only. `set` replaces the full map; `{}` clears it. Unknown actions, empty names, and wildcards fail startup. Names may refer to tools loaded later. Rules are startup configuration, not a live plugin registration API.

These permissions cannot broaden agent tool ceilings. They do not sandbox plugins or intercept direct tool calls, command callbacks, `rness.process`, filesystem access, or network calls inside plugins. No filesystem/network restriction is enabled automatically.


Load the plugin with a `{name='commands', file='./plugins/commands.lua'}` entry in the existing `rness.plugins.setup({...})` list in init.lua. Registration occurs during plugin loading, not inside callbacks.

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

Use `/greet world`, `/help`, or `/help greet`. `arguments` supplies static suffix candidates for the command picker; it does not validate or interpolate input. Candidates update when plugins load or unload. Usage and descriptions generate help automatically. Shell-style argument parsing is not provided.

For runtime candidates, add `complete = function(ctx) return { "world", ctx.session } end` to the declaration. The callback receives the same session/workspace/raw-input context and returns an array of full argument suffixes (without the command name). Press Ctrl+Tab after the command and a space to request completion, then Tab/Enter to select a candidate. Responses for a different draft or session are ignored. Callbacks run off the TUI thread, hold the same reservations as commands, and can be canceled with Ctrl+C. Keep them short and side-effect-free; errors are displayed as session notices. Rust commands override `Command::complete`; integrations call `prepare_command(...).complete(...)`. Static arguments are the fallback when no callback is registered.

Completion arrays may mix strings and `{ value = "a1", description = "scout · Review parser" }`
records. `value` is the full argument suffix; `description` is an optional,
display-only hint that is never inserted or submitted. Check
`rness.commands.completion_descriptions` before returning records from a plugin
that must also run on older binaries. Rust commands may override
`Command::complete_items` to return `(value, description)` pairs, and clients use
`PreparedCommand::complete_items`; existing string-based `complete` methods remain
supported.

`ctx.session` identifies the invoking session. `ctx.workspace` is its saved workspace, or absent for legacy sessions without one. `ctx.raw_input` preserves the text after the command name, including separator whitespace.

A callback may return a string, nil, or `{ message = string, data = JSON-compatible value }`. Errors become submission failures. Results are local notices in the TUI and `{ "status": "command", "result": ... }` over HTTP. They are not appended as model conversation messages. TUI command results are retained per session for the lifetime of the TUI, including results received while that session is hidden. Switching back or reconciling history restores them without duplicates. They are not persisted across process restarts.

Names must start with a lowercase letter and contain lowercase letters, digits, hyphens or underscores. Built-in names are reserved. Duplicate registrations fail. Unload removes owned handlers, metadata and completion entries without removing unrelated commands.

## Execution and cancellation

TUI commands use `SessionService::prepare_command` before scheduling the captured handler on a blocking worker. HTTP submissions use `SessionService::send_async`, which reserves recognized commands synchronously when called, before its returned future is polled. Neither path reinterprets an admitted command as a model prompt. Rust handlers receive a cancellation token and must poll it themselves. The synchronous `send` entry point remains available for synchronous callers.

Admission reserves both the session and extension lifecycle before execution is scheduled. Canceling at this point prevents the callback from starting; Ctrl+C in the TUI cancels admitted commands even without a model turn. Dropping an unpolled submission or prepared command releases its reservations. Handler errors and unwinding panics also release reservations; blocking-worker panics become frontend errors. Dropping a future after the blocking worker has started does not stop that worker: use session cancellation.

Coordinated unload cannot remove a plugin while an admitted command holds its reservation. When the actor processes such an unload request, it returns busy without changing registrations. The Lua actor processes requests serially, so an unload queued behind a running Lua callback waits for that callback to exit before checking the reservation; it may then succeed or return busy. Retry busy unloads after command completion.

Only one command runs per session. Another command or model submission is rejected while it runs. A command is rejected during an active model turn. Extension maintenance cannot acquire its reservation while a command executes. Cancellation uses the ordinary session cancel operation; Lua instruction hooks interrupt Lua execution, including infinite Lua loops. The execution reservation remains held until the handler exits.

Compaction, pruning, configuration changes, agent selection, and command admission share a per-session operation reservation. Conflicting operations fail busy rather than racing the session log. Compaction retains its reservation across the summarizer await; cancellation or failure of that future releases it. The built-in `/agent` uses its existing command reservation. Plugin callbacks cannot reenter public session mutation APIs while holding a command reservation.

`rness.http.get/request` honors command and completion cancellation while waiting for response headers or body by dropping the asynchronous network operation. This releases the Lua actor without waiting for the HTTP timeout. Cancellation does not retract a request already received by the server.

Cancellation is cooperative, not process isolation: arbitrary Rust/native functions, filesystem syscalls, and Lua `io`/`os` operations cannot be forcibly interrupted by a Lua instruction hook. Those operations still require bounded APIs or a separately isolated process; this implementation does not provide a native-code process sandbox. Lua handlers share one actor and do not run concurrently in separate VMs. Do not use blocking native calls for unbounded operations, swallow cancellation errors, or recursively invoke Lua commands through the session bridge. Cancellation does not roll back side effects. Hot reload of registered commands requires restarting; coordinated unload is supported.

## Cancelable external programs

Use `rness.process.run` instead of `os.execute` or `io.popen` for blocking external work:

```lua
rness.commands.register {
  name = "git-status",
  run = function(ctx)
    local result = rness.process.run {
      program = "git",
      args = { "status", "--short" },
      cwd = ctx.workspace,
      timeout_ms = 30000,
    }
    if not result.success then error(result.stderr) end
    return result.stdout
  end,
}
```

The program receives literal arguments; no shell interpolation occurs. Invoke a shell explicitly only when required, and pass untrusted values as positional arguments rather than concatenating a script. `cwd` defaults to the host working directory, so pass `ctx.workspace` for project commands. Stdin is closed, environment is inherited, and no background-job handle is returned.

On macOS/Linux, the child runs in a separate process group. Cancellation or timeout sends SIGKILL to the group and waits for the direct child to be reaped before returning an error. Normal completion also terminates remaining group members. Descendants that deliberately escape with `setsid`/`setpgid` are not contained: this is process management, not a security sandbox. Descendant reaping belongs to their parent or the OS. A kernel-uninterruptible process can delay cleanup; reservations are not released early. Other platforms fail explicitly until a process-tree implementation is available.

The timeout defaults to 30 seconds and must be positive. The result contains `success`, `code` (null for a signal), `signal`, `stdout`, `stderr`, and `truncated`. Nonzero exit codes are returned, not raised. Spawn failures, cancellation, timeout, and detected excessive output raise Lua errors. Output is captured in temporary files and returned as lossy UTF-8, capped at 1 MiB per stream. A running process is checked every 10 ms for excessive output; disk usage may briefly exceed the cap between checks. The same cancellation token applies in command completion callbacks. Calls outside commands/completions still have the timeout, but no session cancellation token.

This closes the supported cancellation contract: Lua callbacks remain cooperative; HTTP and external processes use cancelable host APIs. Arbitrary native code inside the shared VM is deliberately not force-killed, and cancellation never rolls back external side effects.

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
