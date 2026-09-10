# Plugin loading and lifecycle

Plugins are explicitly selected from `init.lua`. Placing a file in `~/.rness/plugins/` does not activate it.

## Enable a plugin

Copy a desired example, such as `examples/plugins/spinner.lua`, into `~/.rness/plugins/spinner.lua`. Review its contents before loading it: plugins execute as trusted Lua code.

Add to `~/.rness/init.lua`:

```lua
rness.plugins.load("spinner")
```

Restart rness. To disable it, remove or comment out the load declaration and restart. The file can remain on disk.

## Execution sequence

1. Evaluate `init.lua` and any modules it explicitly requires in the retained VM.
2. Capture plugin names in load-call order and close startup declarations.
3. Construct providers and mount engine services.
4. Install the engine-backed Lua APIs.
5. Read only selected `plugins/<name>.lua` files and execute them in declaration order in the same VM.
6. Emit `ready`, then synchronize Lua tools into the engine registry.

Plugin top-level code can access mounted session APIs. It cannot add startup declarations, and it should not assume a frontend has already selected a current session. The `ready` hook is not a guarantee that Lua tools have been synchronized yet.

## Ordering and errors

Names contain only ASCII letters, digits, underscores, and hyphens. Do not include a path or extension. Duplicate declarations fail startup rather than executing a plugin twice.

Missing selected files fail startup. Syntax or execution errors are reported per plugin and later plugins are still attempted. On a chunk execution error, queued tools, apps, cards, keymaps, and statusline declarations are discarded and duplicate-name bookkeeping is restored. They cannot be committed accidentally by the next successful plugin. Previously installed implementations remain intact.

Hook subscriptions created during that chunk execution are also unsubscribed on failure, including subscriptions created by required modules or synchronous callbacks during loading. Already cancelled subscriptions are harmless to clean up again. Existing listeners are not removed by cleanup.

This is not a general transaction: explicit cancellation of an existing listener, callbacks already executed, global mutations, module-cache changes, filesystem/network effects, and errors during registration draining are outside that rollback boundary. Successful plugins can be explicitly unloaded through the coordinated TUI operation below.

Tool and app declarations capture their validated fields when submitted, including a detached copy of the tool's JSON schema. Later changes to the caller's table do not change the queued registration. This applies to both registration and explicit replacement. Callback functions themselves are retained, not cloned: mutations to state captured by their closures remain visible when they run.

There is no dependency resolver. Declare prerequisites before consumers and avoid depending on partial initialization of a failed plugin.

## VM ownership and unload

The Rust `LuaRuntime::unload(name)` primitive removes a successfully loaded chunk's current tools, apps, cards, statusline, keymap declarations, and hooks subscribed during its load. It returns `true` once and `false` for an unknown or already unloaded name. Loading the same name twice without unloading is rejected before execution.

Explicit replacement transfers ownership of that registration to the replacing chunk. Unloading an earlier owner cannot remove the later replacement. Unloading a replacement does **not** restore the old implementation; its name becomes available for registration again. Keymaps are different: they are ordered declarations, so removing a chunk's entries leaves earlier bindings available when a host recomputes its keymap.

Ownership means the chunk that registered a callback, not the source file of every function. Tool, app, card, statusline, and hook callbacks retain that owner across later execution, nested event dispatch, and errors. Hooks they create later belong to that same owner and are removed on unload. If another chunk invokes an existing owner's callback while loading, newly subscribed hooks retain the callback's owner; they are also rolled back if the invoking chunk fails. Startup registrations remain outside named-plugin ownership.

Tool/UI/keymap declarations from registered callbacks are rejected immediately, even during synchronous event dispatch at load time. Dynamic hook subscriptions remain supported. Declaration callbacks must not be used as a delayed initialization mechanism.

`LuaHost::unload(name)` is restricted to unmounted hosts. Mounted hosts use `LuaHost::unload_coordinated`, which reserves engine maintenance, unloads the VM registrations, removes tools only when their installed identity still matches, and invokes the frontend cleanup callback before releasing maintenance. Active turns, followups in a running burst, and compactations prevent maintenance; new sends and compactations are rejected while the reservation is held.

In the local TUI, submit `/unload <name>`. Completion polls the actual VM roster, so removed plugins disappear from candidates. A Lua app can return `{ action = "plugin:unload", payload = { name = "spinner" } }` from `on_key` to request the same operation. This does not uninstall files or remove the declaration from `init.lua`; restarting loads explicitly selected plugins again. There is no Lua `rness.plugins.unload` function.

The CLI invalidates cards, statusline and app views, updates the roster and keymaps, and refreshes transcript cards. Generation checks reject stale render and key-action results. Once coordinated teardown is enqueued, dropping the requester does not cancel it. Globals, module caches, and external effects are not undone. Registration-drain failures remain outside the transactional guarantee.

## Why not require everything?

Use `require("connections")` for modules that must execute during startup, including providers, profiles, and agent declarations. Use `rness.plugins.load("spinner")` for a file whose execution should wait until engine mount. These are intentionally different lifecycle operations.

A plugin can use ordinary `require` for its own helpers when it runs. That uses Lua's existing module search and caching semantics, not a second plugin-selection pass.

## Runtime reload

Edits under `plugins/` reload the captured selection in declaration order without
restarting or reexecuting `init.lua`. The existing VM retains startup callbacks,
globals and module caches. Changing `init.lua`, startup modules, or the selected
plugin list still requires restart; unselected files are never activated.

Reload stages tools, commands (including help and completion metadata), apps,
cards, keymaps, statusline and hook ownership. If a selected plugin fails, the
previous runtime registrations remain usable. Successful reload removes old
runtime hooks and invalidates native Plan tokens, retaining the shared Questions
broker and durable session state. The watcher reconciles engine tools while the
maintenance reservation is still held. The TUI polls presentation declarations.
Active engine work rejects reload; save again after it becomes idle to retry.

This is a registration transaction, not a sandbox or rollback of arbitrary Lua:
filesystem/network effects, mutations of shared globals or captured state,
explicit listener cancellation and `package.loaded` changes cannot be undone.
Helpers loaded through `require` remain cached. Keep reloadable declarations in
the selected plugin files and avoid top-level external side effects.

See [init.lua loading rules](../configuration/init-lua.md) for migration and module organization.
