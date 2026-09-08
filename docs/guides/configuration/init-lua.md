# The init.lua entry point

rness evaluates `~/.rness/init.lua` before constructing model providers. This is the startup entry point for personal declarations and Lua registrations. A missing file contributes no declarations.

## Loading rules

| Location | Behavior |
| --- | --- |
| `~/.rness/init.lua` | Evaluated once in the production Lua VM at startup |
| `~/.rness/lua/?.lua` | Available through explicit `require` |
| `~/.rness/lua/?/init.lua` | Available through explicit `require` |
| `~/.rness/plugins/*.lua` | Loaded only when selected with `rness.plugins.load`, after engine mount, in declaration order |
| Repository `examples/` | Copyable material; not automatically loaded |
| `~/.rness/config.lua` | Not a second automatic startup entry point |

The loader prepends personal module paths to Lua's existing `package.path`; it does not remove the preexisting search paths. Do not treat `require` resolution as confined to `~/.rness`.

## Separate declarations into modules

For example:

```text
~/.rness/
├── init.lua
└── lua/
    ├── connections.lua
    └── roles.lua
```

`init.lua`:

```lua
require("connections")
require("roles")
```

Put provider and profile declarations in `connections.lua`, and agent declarations in `roles.lua`. Startup validates referenced profiles after evaluating the entry point. Organize modules so their dependencies remain obvious to readers.

Do not move startup-only declarations into the post-mount `plugins/` directory: declarations have already closed by then.

## Select plugins explicitly

```lua
require("connections") -- executes now, during startup
rness.plugins.load("spinner") -- queues ~/.rness/plugins/spinner.lua
rness.plugins.load("sessions") -- runs after spinner, after engine mount
```

Copy the selected files into `~/.rness/plugins/` first. No directory is scanned for automatic activation. With no load declarations, no plugin files run. Removing a declaration disables that plugin on the next restart without deleting its file.

`rness.plugins.load(name)` is startup-only and returns no value. Names accept ASCII letters, digits, underscores, and hyphens; omit the `.lua` extension. Paths, empty names, and duplicate declarations are rejected. Calls may also live in modules required by `init.lua`; their execution order determines the plugin order.

A missing selected file fails startup with its path. A Lua execution error is reported as a warning and later selected plugins are still attempted; plugin execution is not transactional, so earlier side effects are not rolled back.

This is not equivalent to `require`: `require` executes immediately and uses Lua's module cache and search paths. Plugin selection queues a specific file for execution after the engine APIs exist. Plugins cannot call `rness.plugins.load` to add more plugins after declarations close.

### Migration from automatic loading

Add one explicit load call per desired file previously placed in `plugins/`, in your chosen order. Restart rness. Merely copying a file into that directory no longer activates it. See [plugin lifecycle](../plugins/loading-and-lifecycle.md).

## Startup versus runtime

Provider, model-capability, profile, and agent declarations are startup-only. After startup, their declaration methods fail with a restart-required error. Runtime queries read the captured registry; editing an ordinary Lua table is not a supported way to reconfigure that registry.

The startup VM is retained, so hooks and UI callbacks registered in `init.lua` remain registered. Engine-backed session and subagent APIs are installed later. Do not call them at the top level of startup configuration.

For engine operations that must happen after mount, register a callback:

```lua
rness.hook.on("ready", function()
  local sessions = rness.session.list()
  -- Inspect existing sessions here; do not assume one has been selected.
end)
```

The `ready` callback is not a guarantee that every Lua-defined tool has already been synchronized into the engine registry. Avoid relying on that ordering for immediate named-agent tool validation.

## Applying changes

Restart the process after editing production Lua configuration or plugins. The production host currently rejects reload instead of reexecuting `init.lua` and potentially duplicating registrations. Legacy internal host modes used by tests have different reload behavior; they are not a promise of production hot reload.

## Troubleshooting

- **Startup declarations are closed:** move the declaration into `init.lua` or a module required by it, then restart.
- **Unknown profile:** check the agent's profile reference and confirm the declaring module is required.
- **Nil session/subagent API during startup:** defer engine calls until after mount.
- **Module not found:** check the file path and use the module name without `.lua`.
- **Examples had no effect:** examples are not loaded automatically. Merge the intended configuration into your personal entry point.

See [known limitations](../../project/known-limitations.md) for unresolved lifecycle and compatibility boundaries.
