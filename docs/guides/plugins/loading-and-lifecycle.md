# Plugin loading and lifecycle

Plugins are explicitly selected from `init.lua`. Placing a file in `~/.rness/plugins/` does not activate it.

## Enable a plugin

Copy a desired example, such as `examples/plugins/spinner.lua`, into `~/.rness/plugins/spinner.lua`. Review its contents before loading it: plugins execute as trusted Lua code.

Add to `~/.rness/init.lua`:

```lua
rness.plugins.setup({
  { name = "spinner", file = "./plugins/spinner.lua", watch = true },
})
```

Restart rness. To disable it, remove or comment out the load declaration and restart. The file can remain on disk.

## Execution sequence

1. Evaluate `init.lua` and any modules it explicitly requires in the retained VM.
2. Capture explicit plugin specifications in list order and close startup declarations.
3. Construct providers and mount engine services.
4. Install the engine-backed Lua APIs.
5. Discover selected file/package sources and invoke their setup functions (or inline callbacks) in dependency-first order in the retained VM.
6. Validate central mappings, synchronize Lua tools into the engine registry, then emit `ready`.

Plugin top-level code can access mounted session APIs. It cannot add startup declarations, and it should not assume a frontend has already selected a current session. The CLI synchronizes tools before `ready`; this hook does not imply frontend session selection.

## Ordering and errors

Names contain only ASCII letters, digits, underscores, and hyphens. Do not include a path or extension. Duplicate declarations fail startup rather than executing a plugin twice.

Missing selected files fail startup. Syntax or execution errors are reported per plugin; dependents of a failed plugin are skipped without executing their source, while independent plugins are still attempted. On a chunk execution error, queued tools, apps, cards, keymaps, and statusline declarations are discarded and duplicate-name bookkeeping is restored. They cannot be committed accidentally by the next successful plugin. Previously installed implementations remain intact.

Hook subscriptions created during that chunk execution are also unsubscribed on failure, including subscriptions created by required modules or synchronous callbacks during loading. Already cancelled subscriptions are harmless to clean up again. Existing listeners are not removed by cleanup.

This is not a general transaction: explicit cancellation of an existing listener, callbacks already executed, global mutations, module-cache changes, filesystem/network effects, and errors during registration draining are outside that rollback boundary. Successful plugins can be explicitly unloaded through the coordinated TUI operation below.

Tool and app declarations capture their validated fields when submitted, including a detached copy of the tool's JSON schema. Later changes to the caller's table do not change the queued registration. This applies to both registration and explicit replacement. Callback functions themselves are retained, not cloned: mutations to state captured by their closures remain visible when they run.

Declare prerequisites with a dense name list, such as `dependencies = { "questions" }`.
These are plugin identities, not paths, package-install requests, or nested specs.
Every dependency must be explicitly selected and enabled in the same setup list.
Missing or disabled dependencies and dependency cycles are rejected before plugin
execution. Dependencies load before consumers even when declared later; declaration
order is retained where dependency ordering permits. Nothing is auto-enabled.

For example, copy both bundled files and add these entries to your setup list:

```lua
rness.plugins.setup({
  { name = "plan", file = "plugins/plan.lua", dependencies = { "questions" } },
  { name = "questions", file = "plugins/questions.lua" },
})
```

Questions loads first. Plan review still checks for an available Questions frontend
at runtime: the dependency declaration does not bypass the headless check or imply
approval when no frontend is available.

## VM ownership and unload

The Rust `LuaRuntime::unload(name)` primitive removes a successfully loaded chunk's current tools, apps, cards, statusline, keymap declarations, and hooks subscribed during its load. It returns `true` once and `false` for an unknown or already unloaded name. Loading the same name twice without unloading is rejected before execution.

Explicit replacement transfers ownership of that registration to the replacing chunk. Unloading an earlier owner cannot remove the later replacement. Unloading a replacement does **not** restore the old implementation; its name becomes available for registration again. Keymaps are different: they are ordered declarations, so removing a chunk's entries leaves earlier bindings available when a host recomputes its keymap.

Ownership means the chunk that registered a callback, not the source file of every function. Tool, app, card, statusline, and hook callbacks retain that owner across later execution, nested event dispatch, and errors. Hooks they create later belong to that same owner and are removed on unload. If another chunk invokes an existing owner's callback while loading, newly subscribed hooks retain the callback's owner; they are also rolled back if the invoking chunk fails. Startup registrations remain outside named-plugin ownership.

Tool/UI/keymap declarations from registered callbacks are rejected immediately, even during synchronous event dispatch at load time. Dynamic hook subscriptions remain supported. Declaration callbacks must not be used as a delayed initialization mechanism.

`LuaHost::unload(name)` is restricted to unmounted hosts. Mounted hosts use `LuaHost::unload_coordinated`, which reserves engine maintenance, unloads the VM registrations, removes tools only when their installed identity still matches, and invokes the frontend cleanup callback before releasing maintenance. Active turns, followups in a running burst, and compactations prevent maintenance; new sends and compactations are rejected while the reservation is held.

Unloading a dependency is blocked while a loaded plugin depends on it. Unload consumers first (for example, `/unload plan` before `/unload questions`); there is no cascading unload.

In the local TUI, submit `/unload <name>`. Completion polls the actual VM roster, so removed plugins disappear from candidates. A Lua app can return `{ action = "plugin:unload", payload = { name = "spinner" } }` from `on_key` to request the same operation. This does not uninstall files or remove the declaration from `init.lua`; restarting loads explicitly selected plugins again. There is no Lua `rness.plugins.unload` function.

The CLI invalidates cards, statusline and app views, updates the roster and keymaps, and refreshes transcript cards. Generation checks reject stale render and key-action results. Once coordinated teardown is enqueued, dropping the requester does not cancel it. Globals, module caches, and external effects are not undone. Registration-drain failures remain outside the transactional guarantee.

## Why not require everything?

Use `require("connections")` for modules that must execute during startup, including providers, profiles, and agent declarations. Use an explicit `file` entry in `rness.plugins.setup` for code whose execution should wait until engine mount. These are intentionally different lifecycle operations.

A plugin can use ordinary `require` for its own helpers when it runs. That uses Lua's existing module search and caching semantics, not a second plugin-selection pass.

## Runtime reload

Explicit file and linked-package entries reload only with `watch=true`. File watches
follow the selected file (including atomic replacement); linked-package watches
include Lua helpers and manifest changes. Managed Git packages reject watching;
inline callbacks are not watchable. Unwatched source snapshots are retained, though
setup callbacks can run again when the selected registration set is rebuilt.
Legacy `plugins.load` selections retain directory-based watching. Reload preserves
dependency-first ordering without
restarting or reexecuting `init.lua`. The existing VM retains startup callbacks,
globals and module caches. Changing `init.lua`, startup modules, or the selected
plugin list still requires restart; unselected files are never activated.

Reload validates the dependency graph and stages dependency ownership along with
tools, commands (including help and completion metadata), apps,
cards, keymaps, statusline and hook ownership. If a selected plugin fails, the
previous runtime registrations, dependency edges, and hooks remain usable. Successful reload removes old
runtime hooks and invalidates native Plan tokens, retaining the shared Questions
broker and durable session state. The watcher reconciles engine tools while the
maintenance reservation is still held. The TUI polls presentation declarations.
Active engine work defers reload; the watcher retries automatically without another
save. Session-unloaded plugins are filtered from subsequent watcher reloads and
return only on restart with their activation declaration still present. Failed or
skipped consumers also remain skipped when a prerequisite has been unloaded,
including transitive dependents, so independent plugins can still reload. The
original declared graph is validated before this filtering; missing dependencies
and cycles remain errors even in plugins that would otherwise be skipped.

This is a registration transaction, not a sandbox or rollback of arbitrary Lua:
filesystem/network effects, mutations of shared globals or captured state,
explicit listener cancellation and `package.loaded` changes cannot be undone.
Helpers loaded through `require` remain cached. Keep reloadable declarations in
the selected plugin files and avoid top-level external side effects.

## Explicit setup and owned actions

Call `rness.plugins.setup` once. Entries select exactly one `file`, installed
`package`, or inline `config` function. File and inline sources require `name`;
package sources use the manifest name and must omit `name`. Relative files are
anchored to the configuration directory. `enabled=false` skips source discovery.
`opts` must be JSON-compatible. File/package entrypoints may return a setup function
receiving `(opts, plugin)`; an entrypoint returning nil can register directly but
cannot consume nonempty options. Inline `config` is that setup function itself.
Legacy `plugins.load` cannot be mixed with `plugins.setup`.

```lua
rness.plugins.setup({
  { name = 'review', file = './review.lua', watch = true,
    opts = { prefix = 'Review: ' }, keys = { insert = {'<F8>', '<F9>'} } },
})
```

`review.lua`:

```lua
return function(opts, plugin)
  plugin.action('insert', {
    scope = 'promptbox', description = 'Insert a review prompt',
    run = function(ctx) ctx.promptbox.insert(opts.prefix) end,
  })
  plugin.keys({ insert = { action = 'insert', key = '<F6>' } })
end
```

Action names become `review.insert`. Slots such as `insert` are stable identifiers,
not chord names. Override a slot with a chord, dense nonempty chord list, or `false`;
`keys=false` disables all that plugin's defaults. Unknown slots/actions and invalid
chords fail setup. Declarations belong to the plugin and disappear on unload.

Action contexts expose `session` and buffered `ctx.promptbox.insert(text)`. App-scoped
actions also expose `ctx.app.name` and `ctx.app.close()`. Operations apply as a
validated batch after successful callback completion; failed callbacks discard
operations. Context methods expire when the callback returns. Registration, input,
session/history, and app-activation checks reject stale queued work/results. Any
subsequent terminal input can invalidate a slow action; this is conservative, not a
guarantee that arbitrary Lua side effects can be rolled back.

## Scoped mappings and help

```lua
rness.keymap.setup({
  {scope='promptbox', key='enter', action='core.promptbox.noop'},
  {scope='promptbox', key='f8', action='core.promptbox.submit'},
  {scope='messagebox', key='f9', action='core.messagebox.toggle_tool'},
  {scope='app:review', key='esc', action='core.app.noop'},
  {scope='app:review', key='f10', action='core.app.close'},
})
```

This startup-only API is singular `keymap`, distinct from legacy `rness.keymaps`.
Targets are qualified plugin actions or supported core actions and must match the
scope. Chords are single keys, not sequences: `ctrl+x`, `alt+up`, `<C-x>`, and
`<F8>` are accepted. Explicit conflicting mappings fail; conflicting plugin defaults
are suppressed with diagnostics. A no-op mapping consumes the chord, so moving a
core shortcut requires disabling its old chord separately.

Core global actions: `scroll_up`, `scroll_down`, `scroll_up_page`,
`scroll_down_page`, `cancel_or_quit`, `quit` (prefix `core.`).
Core prompt actions (prefix `core.promptbox.`): `submit`, `newline`,
`delete_previous`, `cursor_left/right/up/down/home/end`, `paste_clipboard`,
`external_editor`, `noop`, `completion_previous/next/accept/dismiss`,
`close_preview`, `preview_up/down/page_up/page_down/start/end`.
Core messagebox actions (prefix `core.messagebox.`):
`previous_thinking`, `next_thinking`, `toggle_thinking`, `previous_tool`,
`next_tool`, `toggle_tool`, `toggle_hidden`, `noop`. Apps support `core.app.close` and
`core.app.noop` with an explicit `app:<name>` scope.

Prompt user mappings precede ordinary editing; plugin defaults do not steal typed
text. Messagebox defaults follow core messagebox controls; user mappings override
those controls. Focused apps and approval modals block global fallthrough. Internal
completion/paste-preview modes accept applicable core mappings and no-op mappings,
not unrelated plugin shortcuts. App Escape remains core-owned unless explicitly
user-overridden. Disabling close requires providing another usable close mapping.

`/help bindings` displays component controls, scoped overrides and diagnostics,
and host fallback mappings. Component defaults and legacy controls are still
separate registries; help is not exhaustive verification of arbitrary plugin
handlers. Apps may publish a `key_help` list of descriptions on registration.

## Validation

Run `cargo test --workspace --all-targets`, `cargo test --workspace --doc`,
`cargo clippy --workspace --all-targets`, and `cargo build -p rness-cli`.
`python3 scripts/plugin_acceptance.py` runs 15 isolated PTY cases against the debug
binary, with temporary HOME directories and no provider submissions. Tests cover
source activation, remap/disable, core submit/no-op startup mappings, and help.
See [known limitations](../../project/known-limitations.md) for certification limits.

See [init.lua loading rules](../configuration/init-lua.md) for migration and module organization.
