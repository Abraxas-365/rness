# Example recipes

Examples are opt-in. Review them before copying into your personal configuration. Files in the repository are never automatically loaded.

## Context injection: time and tmux

The default flavor includes two context-injection plugins that give the model awareness of time and its tmux environment. Both use `pre_step` hooks to inject messages before each model call. No Rust changes are required.

### Time context

`time-context.lua` injects the current local time (with timezone) and elapsed duration since the last message at every model step:

```
Current time: 2026-09-24T15:30:45-0500 (CDT)
Elapsed since last message: 2m 15s
```

The elapsed tracker uses in-process state. In the interactive TUI, it persists across turns within a session. In headless `-p` mode (separate process per turn), elapsed is seeded on the first step of resumed sessions so subsequent steps within that turn show elapsed correctly.

Configure in `init.lua` before the `plugins.setup` call:

```lua
rness.time_context = {
  refresh_interval = 60,  -- seconds; 0 = every step (default)
}
```

A nonzero `refresh_interval` throttles injection: if less than that many seconds have passed since the last injection, the step proceeds without a time message. This reduces context noise in multi-step turns.

The plugin also registers a `turn_end` hook to record when each turn finishes, so the elapsed duration on the next turn's first step reflects the gap between turns, not steps.

### tmux context

`tmux-context.lua` injects the current tmux session, window, and pane at step 1 of each turn:

```
tmux location (turn 1):
session "main", window 2 "code", pane 1 (%42)
window active, pane active
```

The plugin reads `$TMUX_PANE` and queries `tmux display-message` for the seven fields (session name, window index/name, pane index/id, window active, pane active). It is a silent no-op when:
- `$TMUX_PANE` is unset or empty (not running inside tmux).
- The `tmux` command fails or returns unexpected output.

Re-injection is suppressed when the tmux state (everything after the turn header) has not changed since the last injection within the same process. This avoids redundant context in the interactive TUI when the user stays in the same pane across turns.

Configure in `init.lua` before the `plugins.setup` call:

```lua
rness.tmux_context = {
  refresh_interval = 300,  -- seconds; 0 = every turn (default)
}
```

Both plugins are selected in the default flavor's `rness.plugins.setup` list:

```lua
{ name = "time-context", file = "plugins/time-context.lua" },
{ name = "tmux-context", file = "plugins/tmux-context.lua" },
```

Remove either entry to disable that context injection. Neither plugin registers tools, commands, or UI elements — they only inject context through hooks.

## Read-only LSP navigation

Rness exposes the same four semantic operations as DSH: `goToDefinition`, `findReferences`, `goToImplementation`, and `hover`. This is operation-scope parity, not complete provider/lifecycle parity. Rename, formatting, code actions, symbols and diagnostics are not exposed.

Configure servers explicitly in `~/.rness/init.lua`, then restart:

```lua
rness.lsp = {
  timeout_ms = 60000,
  max_locations = 100,
  max_result_chars = 16000,
  servers = {
    rust = {
      command = "rust-analyzer",
      extension_to_language = { [".rs"] = "rust" },
    },
    go = {
      command = "gopls",
      args = { "serve" },
      extension_to_language = { [".go"] = "go" },
    },
  },
}
```

Install the server executables separately and make them available on `PATH`, or use absolute command paths. These are configuration examples, not claims of acceptance testing against those servers. No server is automatically installed or selected. Without `rness.lsp`, the `lsp` tool is absent; the default flavor does not enable it.

Each server accepts `command`, optional `args`, required `extension_to_language`, optional `env`, `initialization_options`, and `configuration`. Extension keys must be lowercase, dot-prefixed and unique across configured servers. Server processes inherit only `PATH`, `HOME`, `USERPROFILE`, `SYSTEMROOT`, `TMPDIR`, `TEMP`, and `LANG`; explicitly add other required variables with `env`. Commands execute directly, not through a shell. `configuration` is returned as a whole for each `workspace/configuration` item; section-specific configuration lookup is not implemented.

### Calling the tool

```json
{"operation":"findReferences","file_path":"src/main.rs","line":12,"character":5}
```

- `file_path` is absolute or relative to the calling session's workspace. It must resolve to a readable UTF-8 file (maximum 4 MB). Paths may resolve outside the workspace; this tool is not a filesystem sandbox.
- `line` and `character` are **one-based**. Character offsets count UTF-16 code units, not bytes or displayed columns. For example, the position after a leading emoji is character 3. Positions inside a surrogate pair are rejected.
- References include declarations. Capability checks return an error when the selected server does not advertise an operation. Missing extension mappings fail instead of falling back to another server.
- Location results contain `kind="locations"`, `locations` (URI and one-based UTF-16 range), `resolvedWorkspaceUri`, and `truncated`. LocationLink results use `targetSelectionRange`. Hover results contain `kind="hover"` and `hover` (null, or contents plus optional range). Results are JSON text; character/location limits preserve valid JSON and mark truncation.

Normal tool permissions and workspace adaptation apply. To defer its schema, add `"lsp"` to `rness.tool_exposure.deferred`. Programmatic calls use `tools.call("lsp", args)` or `tools.parallel(...)` just like other registered tools.

### Lifecycle and current limits

Connections start lazily and are reused per configured server and canonical session workspace. Requests sharing a connection serialize. Each request reads the latest file from disk, sends `didOpen`, performs the query, then sends `didClose`; unsaved editor buffers and persistent document versions are not synchronized. Servers must support UTF-16 and document open/close notifications.

The timeout includes waiting for the connection, startup and the request. Cancellation or transport failure discards/kills the active connection rather than reusing an incomplete exchange; the next call can start a fresh server. Idle connections live with the tool registry. Dropping a synchronized, initialized connection schedules `shutdown` followed by `exit`, bounded to one second before kill/reap. Cancellation sends `$/cancelRequest` for the outstanding ID when the write stream is intact, allows a short exit grace period, then kills/reaps the discarded server. A partial outgoing frame skips protocol cleanup; a partial incoming frame is never reparsed or reused. Cleanup is asynchronous and requires a running Tokio runtime; runtime teardown retains kill-on-drop fallback, not guaranteed graceful shutdown. Dynamic capability registration, diagnostic collection and stderr capture are not implemented. Server-initiated `workspace/applyEdit` is rejected. Configured server executables remain trusted local processes; read-only operations do not sandbox server behavior.

Tests cover the four operations through a real stdio fixture, fresh document contents, location translation, malformed/oversized frames, UTF-16 coordinates, result limits and duplicate extension rejection. Lifecycle fixtures also cover shutdown/exit, hung requests, cancellation IDs, partial-response cancellation, server crashes and fresh connections after failure. Real `gopls`/`rust-analyzer` acceptance remains follow-up work.

## Web search, fetch, and Lua hooks

The default flavor enables independent, keyless `web_search` (DuckDuckGo HTML search) and anonymous `web_fetch`. Search returns links and snippets; it does not call fetch. Fetch reads a known public URL. Remove either configuration section to omit that tool. Search backends never silently fall back.

```lua
rness.web = {
  search = {
    provider = "duckduckgo",
    before = function(request, ctx)
      if request.query:match("^%s*$") then error("Empty search") end
      request.query = request.query .. " official documentation"
      return request
    end,
    after = function(result, ctx)
      -- Mutate the existing array to retain its Lua/JSON array metadata.
      for i = #result.sources, 1, -1 do
        if not result.sources[i].url:match("^https://") then
          table.remove(result.sources, i)
        end
      end
      return result
    end,
  },
  fetch = {
    max_response_bytes = 5000000,
    max_body_chars = 100000,
    timeout_ms = 30000,
    max_redirects = 5,
    after = function(result, ctx)
      result.content = result.content:gsub("\r\n", "\n")
      return result
    end,
  },
}
```

Alternatively replace the search section with **one** of:

```lua
{ provider = "exa", api_key_env = "EXA_API_KEY" }
{ provider = "perplexity", api_key_env = "PERPLEXITY_API_KEY", model = "sonar" }
{ provider = "deepseek", api_key_env = "DEEPSEEK_API_KEY", model = "deepseek-v4-flash" }
```

These three providers require credentials; DuckDuckGo and HTTP fetch do not. DuckDuckGo depends on HTML markup and may return bot challenges/rate limits. Unexpected HTML is an error, not a fabricated empty result. DeepSeek uses its Anthropic-compatible native search endpoint, not chat-completions. Optional `base_url` overrides for paid providers must use HTTPS. Fetch disables ambient proxies/cookies/credentials, validates and pins public DNS addresses, blocks cross-origin redirects and binary responses, and converts HTML to Markdown. This is not complete DSH parity: proxy routing and auxiliary DeepSeek request audit events are not implemented.

### Hook contract

Both sections accept optional `before(request, ctx)` and `after(result, ctx)` functions. Missing callbacks are identity transformations. Configure these in `init.lua`; **restart after changing web configuration or callbacks**. They execute in the real configuration Lua VM, not the isolated `run_code` VM, and are not independently owned plugin registrations.

- Search request: `{ query = string, maxResults = optional_integer }` (`1..100`). Fetch request: `{ url = string }`.
- Search result: `{ sources = array, content = optional_string, truncated = boolean }`. Source fields: `url`, optional `title`, `snippet`, `publishedAt`.
- Fetch result: `{ url = string, contentType = string, content = string, truncated = boolean }`. The URL is the final fetched URL; HTML content is already Markdown.
- Context contains `tool`, and `session`/`call` for dispatched calls. Credentials are not passed to callbacks. Context/request/result are detached values.
- Return a table. Reject with `error("reason")`. Returning `nil`, a scalar, or malformed data fails the tool call. Hook errors surface; they never silently return unprocessed data. `after` runs only after a successful backend operation.
- `before` may rewrite the query or URL. Rust revalidates rewritten requests, including public-address/redirect policy. Normal tool permissions are checked before entering this pipeline.
- `after` may filter/reorder sources or transform content, including replacing content with a summary produced by your own configured summarizer. No summarization model is selected or called automatically. Retained source URLs must come from the original result, without duplicates. Fetch URL/content type and existing truncation metadata cannot be changed. Content and serialized results remain bounded; oversized hook output fails rather than being silently truncated.
- Hooks have a 30-second ceiling and share the operation's overall timeout (fetch: configured value; search: 120 seconds). Cancellation interrupts Lua instruction execution and propagates through the actor token. Arbitrary blocking native Lua calls are cooperative, not forcibly preempted. Keep hooks bounded; do not recursively dispatch web tools or synchronously re-enter the Lua actor. This is trusted configuration code, not a sandbox.
- The external-content warning is added **after** hooks and cannot be removed by returning transformed content. Treat retrieved material as data, never as instructions to reveal secrets or execute commands.

Web tools remain compatible with normal permissions, deferred `ToolSearch` discovery, and `run_code` nested dispatch.

### Plugin-owned hooks

For reloadable hooks, register from an explicitly loaded plugin:

```lua
rness.web_hooks.register("fetch", {
  after = function(result, ctx)
    result.content = result.content:gsub(" +", " ")
    return result
  end,
})
```

One plugin may own each operation (`search` or `fetch`); duplicate ownership fails. Removing a plugin from the reload set removes its callbacks, and failed reloads retain the previous callbacks. Plugin callbacks take precedence over startup callbacks for the whole operation; after unload, startup callbacks apply again. Registration is only allowed while loading a plugin. Each callback invocation is serialized with reload in the Lua actor. A callback already executing finishes before reload; a network request spanning reload uses the newly installed `after` callback when it returns (not a request-wide callback snapshot).

## Tool-pipeline hooks

Every dispatched tool call (native, Lua, MCP, nested `run_code` calls, and subagent sessions) runs:

```
pre_tool chain → [ask → approval] → guard → approval policy → tool_execute(next = tool) → post_tool chain → tool_result
```

```lua
-- Interception hooks are middleware: fn(ev, next). First registered is outermost.
-- Call next() to continue; return a decision to short-circuit; nil = next()'s default.
rness.hook.on("pre_tool", { match = "Bash" }, function(ev, next)
  if ev.args.command:match("rm %-rf /") then
    return { kind = "deny", reason = "refusing to delete /" }
  end
  if ev.args.command:match("^git push") then
    return { kind = "ask", reason = "pushes need a human" }
  end
  return next()                                  -- default { kind = "allow" }
end)

-- Final synchronous veto, after the whole pre_tool chain allowed.
rness.hook.guard(function(ev)
  if ev.tool == "Write" and ev.args.path:match("%.env$") then
    return { kind = "deny", reason = ".env is read-only" }
  end
end)

rness.hook.on("post_tool", { match = "Bash" }, function(ev, next)
  local d = next()                               -- default { kind = "accept" }
  if not ev.result.is_error and #ev.result.output > 20000 then
    return { kind = "accept", content = ev.result.output:sub(1, 20000) .. "\n[truncated by hook]" }
  end
  if ev.result.output:match("FAILED") then
    return { kind = "block", feedback = "Tests failed; fix them before continuing.",
             additional_contexts = { "Run `cargo test` after each change." } }
  end
  return d
end)

rness.hook.on("tool_result", function(ev)        -- observe-only
  rness.log.info(ev.tool .. " -> " .. (ev.result.is_error and "error" or "ok"))
end)

-- Around the tool body: next() runs it and returns { content, output, is_error }.
-- Call next() again to retry, or skip it to short-circuit.
rness.hook.on("tool_execute", { match = "WebFetch" }, function(ev, next)
  local r = next()
  if r.is_error and r.output:match("timed out") then r = next() end
  return r
end)
```

- Payload: `session`, `call`, `tool`, `args` (frozen: arguments cannot be rewritten, and whatever you pass to `next` is ignored), `parent` and `lineage` (delegating ancestors, nearest first; `nil`/empty for top-level sessions). `post_tool`/`tool_result` add `result = { content, output, is_error, duration_ms }`.
- `opts.match` is an anchored regex (Rust syntax) over `ev.tool`. `opts.agent = "<session id>"` limits a handler to that session and its delegated descendants. Hooks without `agent` fire for every session, including subagents. A filtered-out handler is transparent.
- `pre_tool` decisions: `{kind="allow"}` means no objection — it never bypasses the configured approval policy or per-tool rules. `{kind="deny", reason}` fails the call with `reason`. `{kind="ask", reason?}` prompts even under `--approval allow`; the reason is shown in the approval card. A granted ask is that call's approval. With no approver, or under `never`/a `deny` rule, the ask fails closed.
- `guard` returns `{kind="deny", reason}` or `nil`. Guards run in registration order; the first denial wins. A failing guard denies.
- `post_tool` decisions: `{kind="accept", content=?}` or `value=?` (not both) to replace model-visible output (string or content-part array); `{kind="block", feedback}` replaces the result with an error. Both accept `additional_contexts` (string or string array), committed as sourced user messages after the step's tool results, before the next model request. Denied calls also reach `post_tool`.
- Failures: an error thrown in `pre_tool` becomes the call's final error result (no `post_tool`). An error in `post_tool` replaces the result with an error. An unknown decision `kind` counts as a failure. `tool_result` errors are logged only.
- `tool_execute` runs only for calls that passed every gate. It must return `{content = string | parts, is_error = bool?}` (`output` is accepted as a text alias, so `return next()` works unchanged). Returning `nil` after calling `next()` keeps the last run's result; returning `nil` without calling it is an error. An error in the wrapper becomes an error result that still reaches `post_tool`. `next()` yields the VM while the tool runs, so other hooks and parallel calls keep flowing; any other `coroutine.yield` inside a wrapper is an error. Each wrapper segment gets its own 30-second ceiling. If the turn is cancelled after the tool body has run, the call keeps the body's result.
- Each chain runs on the Lua actor with a 30-second ceiling. Turn cancellation interrupts `pre_tool`/`guard`. Parallel calls serialize through the single VM. Do not synchronously dispatch tools from a hook. When no handler is registered for a phase, the phase skips the VM entirely.

**Agent scoping** — all hooks (tool, loop, lifecycle, notification) support `opts.agent`:

```lua
-- Only fires for session "root" and its delegation descendants.
rness.hook.on("pre_step", {agent = "root"}, function(ev, next)
  return next()
end)

-- Same scoping works on notification hooks.
rness.hook.on("session_start", {agent = "root"}, function(ev)
  rness.session.inject(ev.session, "You are the root agent.")
end)
```

The `agent` value is a session ID. A handler fires when:
1. `ev.session` matches the agent ID exactly, OR
2. The agent ID appears in `ev.lineage` (the delegation ancestor chain, nearest first).

This means a handler scoped to a parent session also fires for all its subagent children. When `opts.agent` is omitted, the handler fires for all sessions.

### Loop hooks

These intercept the agent turn loop. Like tool hooks, they use `fn(ev, next)` middleware chains; `ev` always carries `session`, `turn`, `step`.

```lua
-- Reject the step entirely (no model call).
rness.hook.on("pre_step", function(ev, next)
  if ev.step > 20 then return { kind = "reject" } end
  return next()  -- default { kind = "enter" }
end)

-- Inject context into the model request.
rness.hook.on("pre_step", function(ev, next)
  return { kind = "enter", messages = { "Remember: always use British English." } }
end)

-- Observe every model request (currently no config override).
rness.hook.on("request", function(ev, next) return next() end)

-- Retry a non-retryable error.
rness.hook.on("request_error", function(ev, next)
  if ev.error.code == "RATE_LIMIT" then return { kind = "retry" } end
  return next()  -- default: let the engine's retry/fail logic decide
end)

-- Extend a turn that would otherwise stop.
rness.hook.on("turn_stopping", function(ev, next)
  if ev.step == 1 then
    return { kind = "continue", messages = { "Please also write unit tests." } }
  end
  return next()  -- default { kind = "stop" }
end)
```

- `pre_step` decisions: `{kind="enter"}` (default), `{kind="enter", messages={"...", ...}}` to inject context, or `{kind="reject"}` to end the turn. Injected messages are committed as `UserMessage{intent: Inject, source: Hook}`.
- `request` is currently observe-only (`next()` returns nil). A future phase will expose per-step model config overrides.
- `request_error` decisions: `{kind="retry"}` forces a retry even for non-retryable errors (the 500 ms backoff still applies); `nil`/no `kind` defers to the engine's built-in retry budget. The payload includes `ev.error = {code, message, retryable, attempt, max_retries}`.
- `turn_stopping` fires when the model says "end turn" (EndTurn or truncation with no tool calls). Return `{kind="continue", messages={"..."}}` to inject a steer and continue; `{kind="stop"}` (default) closes the turn. Messages are committed as `UserMessage{intent: Steer, source: Hook}`.
- Failure semantics: a throwing `pre_step` is logged and treated as Enter (step proceeds); other hook errors are logged and the default applies. No loop hook failure is fatal to the turn.

### Lifecycle hooks

Notification-only hooks for session and subagent lifecycle events. These fire via `rness.hook.on()` but are not waterfall chains — there is no `next()` or return value.

```lua
-- Fires once per burst (idle → running transition).
-- source: "startup" (first run) or "resume" (subsequent).
rness.hook.on("session_start", function(ev)
  print("session started:", ev.session, ev.source)
  if ev.delegation then
    print("  child of", ev.delegation.parent, "depth", ev.delegation.depth)
  end
  -- Inject context visible to the model on the first step:
  rness.session.inject(ev.session, "Always respond in formal English.")
end)

-- Fires when a subagent child is created (before its first turn).
rness.hook.on("subagent_start", function(ev)
  print("subagent started:", ev.child, "parent:", ev.parent)
  -- ev.agent: agent name (or nil for generic)
  -- ev.mode: "OneShot" or "Continuable"
  -- ev.depth: delegation depth
end)

-- Fires when a subagent settles (one-shot completes, or continuable idles).
rness.hook.on("subagent_stop", function(ev)
  print("subagent stopped:", ev.child, "outcome:", ev.outcome)
  -- ev.outcome: "completed", "aborted", or "error"
end)

-- Fires when a brand-new session is created (not on resume).
rness.hook.on("session_created", function(ev)
  print("new session:", ev.session, "workspace:", ev.workspace)
  -- ev.delegation: nil for root sessions, {parent, depth, mode} for subagents
end)

-- Fires when a session goes idle (turn finished, waiting for input).
rness.hook.on("session_idle", function(ev)
  print("session idle:", ev.session)
end)
```

**`rness.session.inject(session, text)`** — Queue context into a session's next step without waking an idle session or starting a turn. The message is committed as `UserMessage{intent: Inject}`. Useful inside `session_start` to seed model context. Returns `"queued"`, `"started"`, or `"logged"`.

### hooks.json bridge

Declarative command hooks — no Lua required. Place a `hooks.json` in `.rness/hooks.json` (project) or `~/.rness/hooks.json` (user). Format:

```json
{
  "hooks": {
    "pre_tool": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "my-pre-tool-checker", "timeout": 10 }
        ]
      }
    ],
    "session_start": [
      {
        "hooks": [
          { "type": "command", "command": "my-session-logger" }
        ]
      }
    ]
  }
}
```

Each command hook:
- Receives the hook payload as JSON on **stdin**.
- Prints a JSON decision on **stdout** (for interception hooks: `pre_tool`, `guard`, `post_tool`, `tool_execute`, `pre_step`, `request`, `request_error`, `turn_stopping`). Notification hooks ignore stdout.
- Exit code 0 = success; non-zero = error (logged, default decision applied).
- `timeout` is in seconds (default 30).
- `matcher` is optional: when set, only fires for matching `tool`/`source` names (same as `opts.match` in Lua).
- Only `"type": "command"` hooks are supported; other types are logged and skipped.

The bare format (without the `"hooks"` wrapper) is also accepted:
```json
{
  "pre_tool": [
    { "hooks": [{ "type": "command", "command": "echo ok" }] }
  ]
}
```

Project hooks (`.rness/hooks.json`) load before user hooks (`~/.rness/hooks.json`). Both files are optional.

### Hook audit events

Every hook invocation is durably recorded in the session log as paired `hook/invoked` + `hook/result` events:

```json
{"hook/invoked": {"turn": 1, "point": "pre_tool", "source": "lua", "matcher": "Bash", "handler_id": "..."}}
{"hook/result":  {"turn": 1, "point": "pre_tool", "handler_id": "...", "decision": "allow", "duration_ms": 3}}
```

For command hooks, `source` is `"command"` and `hook/result` includes `exit_code` and `stderr_summary` (bounded to 500 chars). These events are audit-only — they never appear in model context.

### Durable background jobs

The CLI persists jobs under `<session-root>/jobs`, including owner, status, output and read cursor. Output is appended and synced to a separate `.output` file; small metadata snapshots are atomically replaced only on lifecycle/read/delivery changes. Abandoned running jobs recover as `interrupted`, never automatically rerun. Process-owner file locks prevent another live Rness instance from being mistaken for a crashed owner; live instances' jobs are not imported. Completion notices carry a stable job ID in the session message's provenance. Admission deduplicates queued IDs and checks durable provenance before retrying, so losing the job-side acknowledgment after the session commit does not append a duplicate. The job is acknowledged only after the session record is visible. This guarantees idempotent session-message insertion under the session service's existing single-writer ownership; it does not promise exactly-once model execution or external side effects. Persistence errors cancel the producer and are logged. Tests cover concurrent retries, lost acknowledgment/restart, output-before-settlement recovery, abandoned partial metadata, append failure, read-cursor recovery and live-owner exclusion. No supervisor or process reconnection is provided.

## Deferred tools and programmatic tool calling

Configure tool presentation in your `init.lua` (restart required):

```lua
rness.tool_exposure = {
  mode = "both", -- "native" (default), "ptc", or "both"
  deferred = { "plugin_echo", "Read" }, -- exact registered names
}
```

- `native` exposes ordinary tool schemas. Names in `deferred` are withheld until discovered.
- `ptc` exposes only `ToolSearch` and `run_code`; ordinary tools are invoked through programs.
- `both` exposes `run_code`, `ToolSearch`, and non-deferred or already-discovered native tools.
- The default flavor remains `native` with no deferred names. Nothing is automatically deferred, including MCP tools.

`ToolSearch` takes `{ "query": "keywords" }` or `{ "query": "select:plugin_echo,Read" }` and returns up to 20 matching full schemas. Keyword searches require every word to match the name/description. Search sees only the calling agent's permitted registry. Successful discovery is logged; native schema activation takes effect on the next model step and survives session replay. Activation is not permission: removed or disallowed tools remain unavailable. `ToolSearch` and `run_code` are reserved engine names, not plugin registration names.

Lua plugin tools registered through `rness.tool.register` work through the same bridge as native tools. For example, a plugin can declare:

```lua
rness.tool.register {
  name = "plugin_echo",
  description = "Echo text",
  input_schema = {
    type = "object",
    properties = { text = { type = "string" } },
    required = { "text" },
  },
  run = function(args, ctx)
    return args.text .. " from " .. ctx.session
  end,
}
```

After discovering its schema, the model can invoke `run_code` with:

```json
{"code":"local results = {}; for i = 1, 3 do results[i] = tools.call('plugin_echo', {text = tostring(i)}) end; return results"}
```

`tools.parallel({{name="Read", args={file_path="a.txt"}}, {name="Read", args={file_path="b.txt"}}})` returns an array of the same result objects in input order. Up to four calls execute concurrently; all entries count against the shared 32-call program budget. Each call retains approvals, restrictions and audit records. An individual tool error does not discard sibling results. Lua-plugin tools still serialize inside their shared actor.

`tools.call(name, args)` returns `{ output, is_error, content }`. A rejected or failed tool returns `is_error=true`; the program must inspect that field. Calls execute sequentially through the existing dispatcher, preserving agent restrictions, approvals, session/workspace context, and cancellation. The nested call ID is derived from the outer call ID. Programs may invoke permitted tools without prior discovery; deferred loading controls schemas, not authority.

Each invocation uses a fresh restricted Lua VM, separate from the configuration/plugin actor. It has table/string/math libraries but no direct filesystem, process, network, package loading, or plugin-global access. Tool implementations retain their usual capabilities; calling Bash is still shell execution governed by its approval policy. This is not an OS process sandbox.

Limits: 64 KiB source, 16 MiB Lua memory, 32 tool calls, and a 60-second cooperative execution budget. Cancellation/time checks occur at Lua instruction hooks and tool boundaries; this is not a hard preemptive deadline for native library operations or non-cooperative tool implementations. Recursive `run_code` and nested `ToolSearch` are rejected. Return values must be JSON-serializable; plugin presentation metadata is not returned to the program.

Nested call starts are appended before dispatch, and results before returning to Lua, as `tools/program_started` and `tools/program_result` audit events. They are not injected as standalone model tool messages: only the outer program result enters model context. Programs are not resumed after a crash. Tool reload/unload is supported between calls after the plugin registry has been synchronized; no guarantee is made for replacing a plugin during an executing program.

## Structured statusline

Declare a one-row statusline at startup or during plugin loading:

```lua
rness.ui.statusline = {
  style = { fg = "#a89984", bg = "#282828" },
  padding = { left = 1, right = 1 },
  separator = " · ",
  left = function(ctx)
    return {
      { text = ctx.activity, style = { fg = "#83a598" } },
      { text = ctx.busy and ctx.elapsed or "" },
    }
  end,
  right = function(ctx) return ctx.model end,
}
```

`left` and `right` accept strings, arrays of `{ text, style }` segments, or
callbacks returning either. Empty segments are skipped. Styles use the same
named groups or inline tables as other UI components. Right content receives
space first; left content is clipped to the remainder with a one-cell gap.
Long right content is clipped too. No wrapping or extra rows are introduced.

Callbacks receive a detached context snapshot: `session`, `model`, `busy`,
`activity` (`working` or `idle`), `elapsed_ms`, and `elapsed` (formatted seconds).
Elapsed time starts when the frontend observes the active session becoming busy.
Mutations to this snapshot cannot change application state. Token usage and
cross-session activity can be cached through hooks, as in the default statusline plugin;
they are not built-in context fields. Callbacks run in the Lua actor outside
terminal rendering, on the existing approximately 500ms refresh cycle.

`visible = false` (or a callback returning false) hides the row. The existing
`rness.ui.statusline(function() ... end)` API remains available: a string uses
the legacy display, while `nil` declines to the built-in statusline. Callback
errors also fall back and are logged. Plugin unload removes its declaration;
in-flight output is discarded when presentation or session context changes.

### Default flavor context usage

The default `statusline` plugin shows two independent segments, for example
`46.3k/150.0k est tok · 120.0k last input`:

- `est tok` is the engine's context pressure estimate (including system/tool
  overhead and reserved output), with the active compaction policy's threshold
  as denominator. It is a **boundary snapshot, not a live token count**:
  `context_usage` frames arrive at the post-prune threshold check and before a
  model request after context reduction. Streaming output and pending tool results
  do not continuously update it. The threshold is not the model's context window.
  The estimate is **calibrated**: each committed step records the meter's
  estimate for its own request, and later measurements are scaled by the
  observed real/estimated ratio (clamped to 0.25–4×) so the heuristic tracks
  the provider's actual tokenizer. Compaction triggers on this calibrated
  pressure. Fresh sessions and legacy logs without a recorded estimate use
  the raw heuristic until the first calibrated step commits.
- With no compaction policy, the estimate has no denominator (`46.3k est tok`).
- `last input` is the latest recorded provider input usage, not an estimate of the
  next request. It is never compared with the compaction threshold. The plugin
  loads it once on first render (including resumed sessions), then refreshes it
  on `step_committed` frames and `turn_end`, rather than replaying logs each render.

Estimates are cached independently per session, including while compacting and
idle. A `history_changed` frame clears that session's stale estimate until the
next pre-step snapshot. Before the first frame, after resume/plugin reload, or
with an older binary that does not emit `context_usage`, the plugin gracefully
shows `? est tok` alongside available `last input` usage; it never substitutes
provider usage or an assumed threshold. These ephemeral estimates are not
recovered from session logs.

The frame wire shape is `{type="context_usage", session=..., estimated_tokens=...,
threshold_tokens=...}`; `threshold_tokens` is optional/null when no policy applies.

## Messagebox presentation

Startup configuration in `init.lua` can style messages and built-in tool cards:

`cache_bytes` sets the retained visual-row allocation budget (default 8 MiB,
range 0–1 GiB; zero disables row retention). Rows farthest from the viewport
are evicted first and regenerated on demand, preserving IDs and heights.
This accounts for vector/string capacities, not allocator overhead. It does
not bound transient rendering of a giant entry, Markdown/card caches, the
history model, or the lightweight per-entry indexes. For example,
`rness.ui.messagebox = { cache_bytes = 2 * 1024 * 1024 }` retains at most
2 MiB of accounted transcript row allocations after each render.

```lua
rness.ui.messagebox = {
  message = { padding = { left = 1, right = 1 } },
  assistant = { style = { fg = '#ebdbb2', bg = '#282828' } },
  tool = {
    display = 'preview', preview_lines = 6,
    header = { show_name = true, show_status = true, show_duration = true },
    arguments = { visible = true, style = 'dim' },
    output = { visible = true, wrap = true },
    states = { running = { style = 'dim' }, error = { style = 'error' } },
  },
  compaction = {
    -- Uses normal tool-card styling unless overridden.
    display = 'preview', preview_lines = 6,
  },
  keys = {
    previous_tool = 'alt+up', next_tool = 'alt+down', toggle_tool = 'ctrl+o',
    previous_thinking = 'alt+p', next_thinking = 'alt+n', toggle_thinking = 'alt+t',
  },
}
```

Each key accepts `false` to disable it. Tool output wrapping affects presentation
only; it never truncates the durable result or changes provider requests.

Compaction summarizes model context, not the visible or durable transcript. A summary
card appears before the compacted messages and inherits `tool` card options unless
`compaction` overrides them. `display` accepts `collapsed`, `preview`, or `expanded`;
`preview_lines` controls the body rows initially shown in preview mode. Use normal
message selection (`select_message`, then `selection_toggle`) to expand or collapse a
selected summary card without changing its configuration.

Styles accept theme group names or direct `fg`, `bg`, `bold`, `italic`,
`underline`, and `reverse` fields. Direct fields patch inherited styles.

Plugins register renderers with `rness.ui.messagebox.tool_card(name, function(call)
... end)`. Legacy `rness.ui.tool_card` registrations still work. The callback gets
`name`, `args`, `output`, `is_error`, and optional `presentation`. Return `nil` to
use the built-in card. Exceptions, cyclic results, excessive nesting, and results
exceeding the 256 KiB structural budget fall back rather than breaking chat.

Presentation metadata is execution-time data, not a request to reread files.
Small Edit/Write snapshots expose `before` and `after`; large Edit results may
contain bounded `hunks` with `old_start`, `new_start`, and `fragment`.
When the byte budget permits, each hunk contains complete affected source lines
(`fragment=false`). Overlapping complete-line windows are merged; replacement
coverage is tracked separately from the resulting hunk count. Otherwise,
`fragment=true` identifies exact replacement substrings, **not complete source lines**.
`truncated=true` means the complete file snapshot was not captured; compare
`captured_replacements` with `replacements` for replacement coverage. Renderers
must handle missing snapshots and old histories without metadata.

Write reads at most 2 MiB + one sentinel byte of the previous file for presentation.
For larger-than-inline snapshots, it removes the common prefix/suffix and captures
a complete-line changed window if its serialized size fits 48 KiB. Such results
have `hunks` and `changes_complete=true`, while `truncated=true` still indicates
that the entire file was not retained. Oversized or unreadable snapshots/ranges
report `changes_complete=false` and `capture_reason`; they are not empty diffs.
The actual write and model-visible result are unchanged by capture limits.

Do not shorten `call.output` before returning a card if expansion should expose
the full output. Use messagebox preview settings instead. Custom renderers own
the whole card, including its header; built-in header options are not injected
into custom cards. Messagebox configuration is startup-only. Renderer selection is user exact, user
wildcard, plugin exact, then plugin wildcard; style-only overrides preserve the
selected renderer. Structured cards may return `{header={left={...},right={...}},
body={...}}` with styled spans, or legacy line arrays. Code/diff blocks use captured
presentation data rather than live file reads. Callback execution has a 100 ms Lua
instruction deadline and 16 MiB incremental VM allocation budget; blocking native
calls are not covered by that deadline.

Tool states `running`, `success`, `error`, and `cancelled` can override presentation.
`collapsed`, `preview`, and `expanded` affect retained output only; expansion cannot
restore output discarded during execution. View state is session-local. Width,
theme, compaction, and height-changing updates can invalidate layout or adjust
suffix indexes; the visual-row budget is not a bound on total transcript memory.

Core messagebox actions also support [scoped user mappings](loading-and-lifecycle.md#scoped-mappings-and-help).
Use `/help bindings` to inspect declared shortcuts and scoped overrides.

## Path references in terminal and HTTP

The default flavor enables this through its `references` plugin. Other flavors can
declare it in a runtime plugin loaded through
a `{name='references', file='./plugins/references.lua', watch=true}` entry in the single `rness.plugins.setup({...})` list in init.lua:

```lua
rness.file_references.enable {
  max_results = 20,
  max_entries = 50000,
  respect_gitignore = true,
  allow_parent = false,   -- Set true for @../ browsing.
  allow_home = false,     -- Set true for @~/ browsing.
  allow_absolute = false, -- Set true for @/absolute/path/ browsing.
  -- Optional replacement for the default directory exclusion list:
  -- excluded_directories = { '.git', 'node_modules', 'target' },
}
```

`rness.file_references.disable()` or unloading the owning plugin cancels indexes,
removes guidance on subsequent steps and clears the terminal picker. Runtime
reload applies changed declarations without restart; failed loads retain the
previous service configuration. An old owner's unload cannot disable a replacement.
In the terminal, type `@` at the beginning
of a token, including within a sentence. Arrows select, Tab drills into a folder,
Enter inserts without sending, and Escape closes the picker. Paths with spaces
are quoted as `@"docs/design notes.txt"`. Only a path is inserted: no file bytes
are attached. Model guidance requires reading files or listing directories when
needed, without claiming they have already been inspected.

Web clients use `POST /api/sessions/:id/file-references` with JSON
`{"query":"src/","limit":50}`. Responses are arrays of `{path,directory}`; no SSE
subscription is needed. The same service uses the session's persisted workspace.
Limits are 1..200. A disabled feature returns an empty array. Queries default to
relative workspace paths. Lua can independently enable `allow_parent`, `allow_home`,
and `allow_absolute` (all default to false). HTTP queries use `../`, `~/`, or `/path/`
without the terminal's `@` prefix. Home expansion uses the server process's `HOME`;
relative paths use the session workspace. Parent traversal within home or absolute
paths also requires `allow_parent`.

External queries browse one directory level at a time and never recursively index
the home directory. Directory symlinks are not traversed; configured directory
exclusions and ignore filters still apply. Selecting a path inserts a reference,
not file contents. These options control discovery, not tool permissions.

An empty query and slash-containing queries list a directory live. Bare queries
use a shared bounded fuzzy path index with deterministic prefix/gap/path ranking.
Only the initial query waits for indexing; stale entries answer while refresh
runs in the background. Tool results invalidate caches; external edits are picked
up by a refresh triggered by queries after 30 seconds. At most eight workspaces
are retained. The entry limit can omit files; raise it if needed.

Default exclusions cover version-control, dependency, cache and build directory
names (including `.git`, `node_modules`, `target`, and `dist`). The optional
`excluded_directories` replaces that list. `respect_gitignore` enables the ignore
crate's Git/global/exclude and `.ignore` rules, including nested rules. Neither
these discovery filters nor relative-path validation change tool permissions.
Directory symlinks are not traversed. Non-UTF-8 paths, quotes and control characters
are omitted. Canceling one caller does not cancel an index another caller shares;
disable/reload cancels its lifetime. Terminal queries reject stale responses.
HTTP workers receive cancellation if their handler is dropped; disconnect
propagation depends on the HTTP server. This exposes a web API, not a web UI.

## Plan mode (initial implementation)

`examples/plugins/plan.lua` enables `exit_plan_mode` and registers
`/plan [on|off|status]`. Copy it and `examples/plugins/questions.lua` into your
configuration's `plugins/` directory, then add both entries to your existing setup
list (do not call setup twice):

```lua
rness.plugins.setup({
  { name = "questions", file = "plugins/questions.lua" },
  { name = "plan", file = "plugins/plan.lua", dependencies = { "questions" } },
})
```

The Questions plugin explicitly calls `rness.questions.enable()` for AskUser and
Plan review; the default flavor selects both plugins rather than enabling Questions
in startup code. `dependencies` lists selected/enabled plugin names, not paths.
Missing or disabled dependencies and cycles are rejected; dependencies load first,
and a failed Questions load skips Plan. Unload Plan before Questions: unloading a
dependency with loaded consumers is blocked. A Questions frontend must still be
available at review time; the headless check remains and missing availability never
implies approval. Tasks is not required and permissions are unchanged.

`rness.plan.enable` accepts optional planning guidance and a dedicated review UI:

```lua
rness.plan.enable {
  -- guidance = "...", -- omit to keep the native planning guidance
  review = {
    title = "Plan review", width = 100, height = 30,
    border = "rounded", padding = { left = 1, right = 1, top = 0, bottom = 0 },
    show_help = true,
    edit_enabled = true, approve_after_edit = true,
    editor = { "nvim" }, -- or { "code", "--wait" }; omit for VISUAL, then EDITOR
    labels = {
      approve = "Approve", feedback = "Request changes",
      edit = "Open in editor", edit_and_approve = "Edit & approve",
      feedback_prompt = "Feedback",
    },
    keys = {
      approve = "a", feedback = "r", edit = "e",
      up = "up", down = "down", page_up = "pageup", page_down = "pagedown",
      first = "home", last = "end", dismiss = "esc", cancel = "ctrl+c",
      feedback_submit = "enter", feedback_back = "esc",
    },
    styles = { border = { fg = "#83a598" } },
  },
}
```

All fields are optional; the values above (except `editor` and `styles`) are the
defaults. Edit this declaration in `plugins/plan.lua`, rather than enabling Plan
again in a second plugin. Partial labels/keymaps keep omitted defaults. Dimensions
are terminal cells; width/height clamp to the available terminal. Border, padding,
and style names use the Questions UI vocabulary. Plan review styles are independent
of ordinary AskUser. Invalid options, conflicting keys, and unusable dimensions
are rejected at plugin load. Ctrl+C remains a cancellation safety binding.

The review shows Markdown and the approve, feedback, and editor actions together,
without the generic AskUser heading or a separate choices screen. Press `e` to
open a temporary `.md` file. **A successful editor exit immediately approves the
file's contents**, even if unchanged; no second approval is required. The edited
plan replaces the original in the durable tool result seen by the agent. Save
before exiting. Use a waiting editor command (`code --wait`, not just `code`);
argv is executed directly, not through a shell. VISUAL/EDITOR are treated as an
executable path; use the Lua argv for flags. The temporary file is removed afterward.
To abort editing without approval, exit the editor with a failure status (e.g.
Vim `:cq`). Missing editors, nonzero exits, unreadable files, or invalid Markdown
leave the review open. Dismissed/cancelled/stale reviews never accept an editor
completion. Set `approve_after_edit = false` to return to the edited preview for
explicit approval instead, or `edit_enabled = false` to hide/disable editing.

Remote frontends receive `plan_review` metadata with the question and may submit
`edited_markdown` alongside `selected = { "Approve" }` (no custom feedback).
Edited Markdown is accepted only for an editable plan review and must start with
a nonempty `# ` heading; ordinary AskUser answers cannot attach edited plans.

`rness.session.plan(id)` reads `{active, pending}` and
`rness.session.plan(id, true_or_false)` selects the next mode.
Selections remain in memory until the next uncancelled step boundary, where
`plan/mode` is appended through the turn writer. A selection alone does not
start a turn, survive restart, or enter a fork. Committed mode does survive.

The review requires a complete Markdown plan starting with `# `. Exactly one
Approve selection without custom feedback produces a durable approved tool
result. At the following step boundary the engine logs the exit and continues
without another user message. Keep planning returns feedback and retains the
mode. Dismissing review ends the turn without another model request. Already
running sibling tools are not rolled back or blocked. Cancelled reviews do not
approve. A fork after approval but before the next step inherits the pending
exit from the durable result. Same-step provider retries reuse their prompt.

`rness.plan.disable()` is a transactional load-time declaration: it removes the
native review tool and cancels its live review token without erasing session
history. A failed plugin chunk does not apply the disable. Enable and disable
cannot be combined in one load. An old owner's unload cannot remove a newer
registration. Disabling the mechanism does not remove commands owned separately
by another plugin; unload the example to remove its `/plan` command too.

Runtime plugin edits reload while idle, including the `/plan` command, without
reexecuting init.lua. Successful reload invalidates old Plan tokens and uses the
live Questions broker; failed declarations preserve the prior registrations.
Startup configuration and module changes still require restart. See
[reload guarantees and limits](loading-and-lifecycle.md#runtime-reload).

The Questions frontend opens Plan reviews in a Markdown reader, using the same
styled renderer as chat (headings, lists, emphasis, and code blocks). The native
request carries the complete source in the optional `markdown` field, separate
from the approval question, including over HTTP/SSE.

Up/Down or j/k scroll one line, PgUp/PgDn scroll a page, and Home/End jump to the
bounds. Resizing reflows the document and clamps scrolling. Enter or Tab opens
the choices without approving; select Approve and submit explicitly to proceed.
Choose Request changes / feedback to type a revision request. PgDn from choices
reopens the document. Escape dismisses from either view; Ctrl+C cancels the turn.
The reader and explicit approval were checked in real tmux at 80x24 and 40x12.
`/plan status` reports active and pending state; there is no separate
always-visible Plan status widget.

## Startup modules

Copy selected files from `examples/lua/` into `~/.rness/lua/` and require them from `init.lua`:

```lua
require("providers")
require("roles")
```

- `providers.lua`: explicit Anthropic, OpenAI API, ChatGPT OAuth (named `chatgpt`), DeepSeek, OpenCode Go (Chat Completions and Messages), Ollama, OpenRouter, and Groq connections. There are no built-in provider registrations: load this module, register your own connections, or pass `--route`. Remote credentials come from environment variables or explicit credential-store references; Ollama requires a running server and installed model. No profiles or defaults are imposed. Remove duplicate provider registrations when combining with `examples/init.lua`.
- `roles.lua`: a tool-free principal planner and a delegable implementer. No mandatory profile and no implicit default selection.
- `examples/init.lua`: broader connection, profile, authentication, and agent configuration examples. Merge selected declarations instead of overwriting your configuration.

## Pasted text in the terminal

Bracketed pastes preserve their content and never submit automatically. More than five lines or 500 characters collapse into an atomic draft block. Move the caret beside a block and press Ctrl+G to preview; arrows, PageUp/PageDown and Home/End scroll. Escape closes the read-only preview. Ctrl+E opens the entire prompt in an external editor, including from preview or an empty draft. All paste blocks expand in order with surrounding text. Save and exit replaces the draft with plain text, discarding the old block boundaries without sending; nonzero exit or invalid UTF-8 leaves the original draft intact. Backspace beside a block removes it as a unit. Draft history retains blocks; submission expands the original text, not labels.

Optional startup configuration in `init.lua`:

```lua
rness.ui.promptbox = {
  editor = { "nvim", "--clean" },
  keys = { edit = "ctrl+e" },
  paste = {
    lines = 5,
    chars = 500,
    keys = { preview = "ctrl+g" },
  },
}
```

Editor configuration is an argument vector, not a shell command. If omitted, `$VISUAL`, then `$EDITOR`, supplies an executable name (use the explicit vector for arguments). A private temporary `.txt` file is removed after editing. The TUI suspends terminal input while the editor owns stdin and restores the screen afterward. Both shortcuts belong to this component configuration, not `rness.keymaps`. Set either shortcut to `false` to disable it. Ctrl+E takes precedence over the composer's end-of-line binding; disabling or moving it restores that binding. Alt+Enter inserts a newline. Multiple large pastes remain independent blocks until full-prompt editing replaces the draft. Startup configuration changes require restart.

Currently previews are plain text, and the durable transcript receives expanded text rather than persisted paste metadata.

## Image attachments (partial implementation)

Ctrl+V pastes from the local OS clipboard: images become attachments; otherwise text uses the same insertion and large-paste collapsing as terminal bracketed paste. Neither sends the prompt. Configure this shared action with `ui.promptbox.keys.paste` (or `false` to disable); there is no separate image-paste binding. Alt+Right cycles the selected image (dimensions and byte size are shown). Alt+Backspace removes the selected image. Enter submits all attached images with the prompt; Ctrl+E edits only text and preserves attachments. Rejected submissions retain the draft. Clipboard access refers to the machine running rness, not the SSH client's clipboard. Alt+P toggles a raster thumbnail of the selected clipboard attachment; Esc closes it. The preview uses truecolor half-block cells (no Kitty/Sixel requirement), composites transparency over black, and never sends the draft. Preview and close bindings are configurable below.

```lua
rness.ui.promptbox = {
  editor = { "nvim" },
  keys = { paste = "ctrl+v", edit = "ctrl+e" },
  images = { keys = { next = "alt+right", remove = "alt+backspace", preview = "alt+p", close = "esc", history = "alt+h" } },
}
rness.images = {
  normalize_srgb = true,
  animation = "first_frame", -- or "reject"
  deepseek_files = false, -- opt-in remote upload reuse on api.deepseek.com
  anthropic_files = false,
  max_request_images = 20,
  max_request_bytes = 20971520,
  max_input_bytes = 20971520,
  max_input_pixels = 40000000,
  max_pixels = 4000000,
  max_dimension = 4096,
  max_bytes = 5242880,
  lossless = true,
  quality = 85,
}
```

Images are stored alongside credentials in the `images` directory, with durable content-addressed references in session history. Sources are preserved; request variants apply orientation, resize without upscaling, and strip metadata through re-encoding. Lossless images use PNG; lossy transparent images use WebP and opaque images use JPEG. Results over the byte limit are rejected. Variant caching avoids repeated transformation/encoding, not retransmission: by default adapters inline retained images in each request. By default, request processing converts supported embedded ICC profiles or decoded color-space metadata to sRGB. Invalid/unsupported profiles fail explicitly; `normalize_srgb = false` disables conversion. Animation defaults to the decoded first frame; `animation = "reject"` rejects animated GIF, APNG, and WebP requests. Source animation and metadata remain intact in storage. Cached variants skip transformation/encoding, but sources are still read and validated.

Before serialization, all three adapters apply `max_request_images` and `max_request_bytes` to image occurrences (including MCP tool results). The byte budget counts base64-encoded processed images, not the entire JSON request or tokens. Oldest occurrences become explicit text placeholders until both budgets fit; original session history and files are unchanged. Identical image references count separately when repeated. A budget smaller than one image can omit every image. Reattach an omitted image when it is needed again. Retained images still appear in each request; this is bounded context, not send-once semantics.

HTTP clients POST raw bytes with an image Content-Type to `/api/sessions/:id/images` (20 MiB transport cap), then submit `{kind="image", attachment=<returned reference>}` as content. GET `/api/sessions/:id/images/:image` retrieves images already referenced in that session. Upload ownership and stored bytes are validated before submission. As with the existing server, deployment authentication must protect the API.

Model declarations may set `capabilities.image_input = false` to forbid images at submission; absent declarations remain unknown. Resolved text-only providers also reject image-bearing context before every request, including images returned by tools during an active turn. The rejection is non-retryable and tool results remain in durable history.

MCP dispatcher calls preserve ordered text/image blocks and `isError`, admitting decoded images through the shared store off the async executor. Invalid base64, MIME, image data, and configured input limits produce tool errors. Session history stores references, not base64; referenced tool images are retrievable through the same HTTP endpoint. Anthropic nests rich content inside `tool_result`; Responses uses rich `function_call_output`; Chat Completions retains tool replies and adds a call-ID-labelled user image message after the tool-result group. Direct text-only MCP execution still rejects image results. The text-only pruner skips mixed image results to preserve ordering.

Runtime plugins can call `rness.images.policy()` and `rness.images.configure({ quality = 70, lossless = false, max_request_images = 5 })`. Updates are validated and affect subsequent requests, including already-created providers. This is a process-wide policy update, not a per-session transform callback. Admission byte/pixel limits remain startup-only; unknown fields and invalid values are rejected.

Set `anthropic_files = true` in the startup image policy or runtime configure call to opt into Anthropic Files API reuse with API-key authentication. Other adapters stay inline; Anthropic OAuth rejects this option explicitly. Uploads request a one-hour remote expiration; cached IDs are reused within the process for less than that duration and scoped by endpoint, credential digest, MIME, and processed bytes. A metadata 404 causes re-upload. Restarting reloads unexpired IDs from the atomic on-disk upload cache under the image store; keys contain only digests, never raw credentials. Uploads are workspace-visible and not eligible for ZDR; file references still incur image input tokens. See [Anthropic Files API](https://platform.claude.com/docs/en/build-with-claude/files). No live cloud upload was performed during QA.

Alt+H opens historical images (including HTTP-submitted and MCP images); Alt+Right cycles and Esc closes. This viewer is read-only and does not attach history images to the draft. Real macOS clipboard text/image intake, preview rendering, and closing were exercised in the `image-qa` tmux window using an isolated local configuration; prior clipboard contents were restored. Automated tests cover rendering, draft preservation, provider serialization, budgets, runtime policy validation, and upload reuse/404 recovery.

Set `deepseek_files = true` to use DeepSeek's multipart Files API (`purpose=user_data`, one-hour expiry) and `type=file` message parts. Enabled only for `api.deepseek.com` and localhost test endpoints; other compatible endpoints remain inline. IDs are cached by endpoint, credential digest, and processed image, expire according to the provider response, and are re-uploaded after metadata 404. Tests verify repeated requests upload once and recover missing files. No live DeepSeek request was made.

Runtime plugins may register encoded-byte processors using `rness.images.processor("my-transform-v1", [[return function(metadata, bytes) return bytes end]])`. The source evaluates to a callback in a separate Lua VM, receives the image reference metadata and original encoded bytes, and returns encoded image bytes. The output is validated and goes through normal color/resize/encoding limits. Increment the version whenever behavior changes; it participates in the variant cache key. `rness.images.processor("", nil)` removes the callback. This is a source-defined callback, not a closure over plugin state; no `rness` services are installed in its VM. Lua allocations are limited to 128 MiB and instruction hooks enforce execution limits (not a hard sandbox for blocking native library calls).

With `lossless = false`, transparent images use WebP at effort 0 and qualities 85/75/60 (capped by configured quality); opaque images use JPEG with the same ladder. `lossless = true` retains PNG. The byte limit remains hard.

Both Files adapters recover once from recognized storage/file-quota errors. Successful new uploads create atomic ownership records scoped by endpoint and credential digest. Cleanup inspects at most 4096 local records and deletes at most 16 oldest eligible owned uploads, even before expiration. Ownership-record modification time (written at upload, not reuse) determines age, independent of expiration. Files already selected for the current request are protected; unrelated account files and legacy caches without ownership records are never deleted. No remote listing or pagination is needed. DELETE 404 retires stale ownership; other cleanup failures stop recovery. Cancellation and HTTP timeouts apply. If every candidate is protected, the request reports quota exhaustion. Local image originals and variants are untouched.

Evicted cached IDs are detected by metadata lookup and re-uploaded when next needed. Both Files adapters also recover once when the model endpoint returns a recognized stale-file HTTP error (400/404/410/422), before streaming begins. The retry invalidates matching cache-key/file-ID generations, resolves/uploads the images again, and resends. If the error names no used ID, all used mappings are treated as stale. A concurrently replaced cache generation is preserved. Repeated rejection fails normally; stream errors are not automatically replayed, preventing duplicated output. Upload deduplication remains process-local and there are no cross-process leases: eviction races remain possible, but now have bounded model-request recovery as in DSH.

## Runtime plugins

Copy desired files from `examples/plugins/` into `~/.rness/plugins/`, then select them:

```lua
rness.plugins.setup({
  {name='text-tools', file='./plugins/text-tools.lua', watch=true},
  {name='bottomline', file='./plugins/bottomline.lua', watch=true},
  {name='session-log', file='./plugins/session-log.lua', watch=true},
})
```

| Recipe | Demonstrates |
| --- | --- |
| `text-tools.lua` | A pure `text_stats` tool with JSON Schema, runtime input validation, and deterministic output |
| `bottomline.lua` | A statusline callback with event-driven running-session counts |
| `session-log.lua` | Turn hooks that log metadata without logging prompts or tool arguments |
| `keymaps.lua` | Deferred setup options, an owned prompt action, and a remappable/disableable binding slot |
| `diffcards.lua` | Custom tool-result presentation |
| `tree.lua` | Stateful sidebar application |
| `sessions.lua` | Session-selection overlay |
| `branches.lua` | Branch navigation application |
| `mcp-clima.lua` | Optional MCP server connection; inspect its external script prerequisite |
| `time-context.lua` | `pre_step` hook injecting current time and elapsed duration; configurable throttle |
| `tmux-context.lua` | `pre_step` hook injecting tmux session/window/pane; silent outside tmux, change-suppression |

Use `bottomline` as an alternative to the spinner, not as a second independent bar. The current API is `rness.ui.statusline`, not `rness.ui.bottomline`. Its latest-session label comes from events and is not a guarantee of the currently selected frontend session.

`text_stats` measures bytes, not Unicode characters. Its words are whitespace-delimited. A trailing newline produces a final empty line. Invalid input becomes a tool error; the example does not assume JSON Schema alone enforces inputs.

## Live Lua applications

`rness.ui.app` supports command-driven opening and opt-in refresh in the local TUI:

```lua
rness.ui.app {
  name = "activity", slot = "overlay", title = "Activity",
  refresh_ms = 500, -- optional integer 50..60000; only refreshes while visible
  capture_escape = true, -- optional; let on_key handle Back before close
  config = {
    width = 100, height = 24,
    style = "overlay", title_style = "title",
    border = { kind = "rounded", style = "dim" },
  },
  view = function(ctx) return { "Session: " .. ctx.session } end,
  on_key = function(key, ctx)
    -- false/nil passes; an unhandled Escape closes a captured app.
    return false
  end,
}
rness.commands.register {
  name = "activity", allow_busy = true,
  run = function(ctx)
    return { data = { action = "app:open", app = "activity", session = ctx.session } }
  end,
}
```

Views receive `session` and available content dimensions `rows`/`cols` (dimensions
may be absent before the first render). Return plain strings, not ANSI markup.
The renderer sanitizes terminal control sequences. `config.width`/`height` are
positive cell dimensions clipped to the available slot; overlay width is centered.
Border kinds are `none`, `plain` (default), `rounded`, and `double`. Styles resolve
against the active theme. Without explicit height, the legacy content-sized panel
is retained. Without `refresh_ms`, apps remain event-driven. Configuration is
snapshotted when declared; later mutations to the supplied table do not change
the registered app. Existing custom apps that depend on background changes should
set `refresh_ms` explicitly; roster polling is no longer an implicit refresh source.
The shipped Tasks, Sessions, and Branches views opt into 500 ms refresh. Update
those copied plugins when upgrading an existing configuration.

Refreshes are serial and stop when hidden. Session switches close the app and
invalidate pending views/keys. Opening is restricted to registered apps in the
command's invoking session; command data is not a general action dispatcher.
Legacy apps close on Escape locally. `capture_escape=true` opts into Lua handling,
with host close on a passed Escape; explicit user key bindings still take priority.
Use `rness.ui.replace_app` for declaration-phase replacement. The jobs plugin is a
complete example with [configuration and callbacks](../background-jobs.md).

## Persistent tasks

Copy `examples/plugins/tasks.lua` into `~/.rness/plugins/tasks.lua` and add
`{name='tasks', file='./plugins/tasks.lua', watch=true}` to the existing setup list in `init.lua`. It enables the Rust `TaskWrite`
tool and an optional Ctrl+T overlay. Nothing is enabled automatically.

A minimal tool-only plugin is:

```lua
rness.tasks.enable { allow_parallel_in_progress = true }
```

`allow_parallel_in_progress` defaults to true. Set it to false to permit at
most one `in_progress` item per snapshot. Configuration rejects unknown keys.
Call enable or disable once per plugin load, not inside runtime callbacks.
`rness.tasks.disable()` removes the native tool on registration reconciliation;
it does not erase saved state or another plugin's separately registered view.
Unloading the example (`/unload plugins/tasks.lua`) removes both its tool and
view. Re-enable by loading the plugin again. Failed plugin chunks discard
pending declarations. `TaskWrite` is reserved in the Lua tool registry.

The model replaces the entire list with:

```json
{"tasks":[{"id":"tests","content":"Run regression tests","status":"in_progress"}]}
```

IDs and content must be nonblank; IDs must be unique within a snapshot.
Statuses are `pending`, `in_progress`, and `completed`. The model preserves IDs
for continuing tasks. Omitted items disappear; `{"tasks":[]}` clears the list.
This is an agent-owned list, not a checklist that silently constrains execution.

### Persistence and concurrency

A successful task tool result carries an optional typed `tasks` snapshot in the
same `tool/result` JSONL envelope. The existing turn writer appends and fsyncs
both together: there is no second database or second writer. Failed validation
and cancelled calls carry no state. Once a successful result is committed,
later cancellation does not roll it back. A crash before its envelope commits
leaves the preceding snapshot authoritative.

Multiple calls in a step execute concurrently, but results commit in model
order. Last committed whole-list snapshot wins, not whichever worker finishes
last. There is no merge or optimistic version conflict API. Task state derives
from the complete fork-resolved history: resume restores it, forks inherit only
their selected prefix, and compaction/pruning do not erase it. An empty latest
snapshot remains authoritative. New sessions start empty.

When TaskWrite is advertised, each model step receives the latest durable
snapshot as labeled task data in its system prompt, including after compaction.
Same-step retries reuse that snapshot. Disabling the tool removes this extra
context on subsequent turns; ordinary historical tool results remain history.
Existing tool ceilings still control visibility; tasks add no permission rules.

### Reading and presentation

- Rust: `SessionService::tasks(session)`.
- Lua: `rness.session.tasks(session)` returns `{ tasks = {...} }` even when the
  tool is unloaded.
- HTTP: `GET /api/sessions/:id/tasks` returns the same snapshot.
- SSE: the existing `HistoryChanged` frame is emitted after a task-bearing
  result commits. Fetch the snapshot again; reconnecting consumers should
  reconcile rather than treating ephemeral notifications as storage.

The example overlay uses this same service projection, with per-session wrapped-line
scroll positions. j/k and arrow keys scroll one line; Page Up/Down scroll a page;
Home/End reach the first/last page; Escape closes the overlay. Continuations are
indented, status markers remain on the first line, and the header shows the visible
line range. Long words are split without dropping their remaining text.

External apps receive `ctx.cols` and `ctx.rows` (content area inside borders),
absent before the first render. Viewport changes request a fresh view, including
live terminal resize. `rness.ui.wrap(text, columns)` returns display-width-aware
wrapped lines using the host's textwrap implementation; columns must be positive.
Control characters are rendered as spaces. The task example reserves its desired
height with blank lines to avoid shrinking its layout to a previously clipped
viewport. Its keybinding and presentation remain Lua policy.

The HTTP endpoint is read-only; task mutations occur through model tool dispatch.
Plan mode is a separate feature and is not enabled by tasks.

## Messagebox reconciliation and rendering limits

The TUI requests only envelopes after its last durable event ID. The reader caches
complete JSONL lines and retries incomplete tails. Unknown cursors and compaction
reload history; provider context construction still uses full history where required.
Append-only visual updates reuse cached rows and extend the height index without
walking the cached entry prefix. Changes to width, theme, card publications, or view
state can still cause a full cache check. The transcript row cache is not memory-bounded.

Structured cards may omit `header` and supply only `body`. Newlines in body spans
split into separate rows while retaining span styles. Custom card callbacks still
run in the Lua host: output size/depth limits are not execution-time limits.

Large-file snapshots are intentionally bounded and may be incomplete. Inspect
`truncated`, `changes_complete`, and capture metadata rather than treating displayed
hunks as a complete file diff. Write captures separated changed ranges within its
48 KiB budget using a time-bounded diff when a single changed window is too large.
Edit merges overlapping complete-line windows; fragment captures remain explicit.
The original plan is not fully accepted.

## Model capabilities

Copy `examples/lua/models.lua` to `~/.rness/lua/models.lua` and call `require("models")` from `init.lua`. Do not select it with `rness.plugins.setup`: capabilities are startup declarations and changes require restart. The example uses `rness.models.declare { provider=..., model=..., capabilities=... }`, with `max_output_tokens` and structured `reasoning.efforts` / `reasoning.budget_tokens`.

Runtime readers `rness.models.get("provider/model")`, `rness.models.list()`, and `rness.models.capabilities(provider, model)` use the same startup registry as request validation. `get` returns nil for unknown models, `list` returns sorted names, and returned tables are copies. Spinner/autocompact therefore use the declared context window, retaining their fallback for unknown models. Runtime plugin reload does not change these facts. Declarations do not register providers, authenticate accounts, or set request defaults; verify gateway/account limits before enabling the catalog.

These recipes cover several extension patterns, not every built-in tool or Lua namespace. No new examples are enabled in personal configuration automatically.

See [plugin lifecycle](loading-and-lifecycle.md) for execution order and restart requirements.
