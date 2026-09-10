# Example recipes

Examples are opt-in. Review them before copying into your personal configuration. Files in the repository are never automatically loaded.

## Path references in terminal and HTTP

Declare in a selected runtime plugin (off by default), loaded through
`rness.plugins.load('references')` in init.lua:

```lua
rness.file_references.enable {
  max_results = 20,
  max_entries = 50000,
  respect_gitignore = true,
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
Limits are 1..200. A disabled feature returns an empty array. Queries must be
relative workspace paths; this discovery boundary does not change tool permissions.

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
`/plan [on|off|status]`. Copy and explicitly load it as a runtime plugin,
after session services and the Questions broker have been installed. A
Questions frontend must be enabled to review plans; missing availability never
implies approval. Tasks is not required and permissions are unchanged.

`rness.plan.enable { guidance = "..." }` accepts optional planning guidance;
omitting it uses the native default. `rness.session.plan(id)` reads `{active,
pending}` and `rness.session.plan(id, true_or_false)` selects the next mode.
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
require("additional-providers")
require("roles")
```

- `additional-providers.lua`: explicit OpenCode Go (Chat Completions and Messages), Ollama, OpenRouter, and Groq connections. Remote credentials come from environment variables; Ollama requires a running server and installed model. No profiles or defaults are imposed. Remove duplicate provider registrations when combining with `examples/init.lua`.
- `roles.lua`: a tool-free principal planner and a delegable implementer. No mandatory profile and no implicit default selection.
- `examples/init.lua`: broader connection, profile, authentication, and agent configuration examples. Merge selected declarations instead of overwriting your configuration.

## Runtime plugins

Copy desired files from `examples/plugins/` into `~/.rness/plugins/`, then select them:

```lua
rness.plugins.load("text-tools")
rness.plugins.load("bottomline")
rness.plugins.load("session-log")
```

| Recipe | Demonstrates |
| --- | --- |
| `text-tools.lua` | A pure `text_stats` tool with JSON Schema, runtime input validation, and deterministic output |
| `bottomline.lua` | A statusline callback with event-driven running-session counts |
| `session-log.lua` | Turn hooks that log metadata without logging prompts or tool arguments |
| `keymaps.lua` | Host action keybindings |
| `diffcards.lua` | Custom tool-result presentation |
| `tree.lua` | Stateful sidebar application |
| `sessions.lua` | Session-selection overlay |
| `branches.lua` | Branch navigation application |
| `mcp-clima.lua` | Optional MCP server connection; inspect its external script prerequisite |

Use `bottomline` as an alternative to the spinner, not as a second independent bar. The current API is `rness.ui.statusline`, not `rness.ui.bottomline`. Its latest-session label comes from events and is not a guarantee of the currently selected frontend session.

`text_stats` measures bytes, not Unicode characters. Its words are whitespace-delimited. A trailing newline produces a final empty line. Invalid input becomes a tool error; the example does not assume JSON Schema alone enforces inputs.

## Persistent tasks

Copy `examples/plugins/tasks.lua` into `~/.rness/plugins/tasks.lua` and add
`rness.plugins.load("tasks")` to `init.lua`. It enables the Rust `TaskWrite`
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

## Model capabilities

Copy `examples/lua/models.lua` to `~/.rness/lua/models.lua` and call `require("models")` from `init.lua`. Do not select it with `rness.plugins.load`: capabilities are startup declarations and changes require restart. The example uses `rness.models.declare { provider=..., model=..., capabilities=... }`, with `max_output_tokens` and structured `reasoning.efforts` / `reasoning.budget_tokens`.

Runtime readers `rness.models.get("provider/model")`, `rness.models.list()`, and `rness.models.capabilities(provider, model)` use the same startup registry as request validation. `get` returns nil for unknown models, `list` returns sorted names, and returned tables are copies. Spinner/autocompact therefore use the declared context window, retaining their fallback for unknown models. Runtime plugin reload does not change these facts. Declarations do not register providers, authenticate accounts, or set request defaults; verify gateway/account limits before enabling the catalog.

These recipes cover several extension patterns, not every built-in tool or Lua namespace. No new examples are enabled in personal configuration automatically.

See [plugin lifecycle](loading-and-lifecycle.md) for execution order and restart requirements.
