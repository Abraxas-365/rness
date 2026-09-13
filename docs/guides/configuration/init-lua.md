# The init.lua entry point

rness evaluates `~/.rness/init.lua` before constructing model providers. This is the startup entry point for personal declarations and Lua registrations. A missing file contributes no declarations.

## Loading rules

| Location | Behavior |
| --- | --- |
| `~/.rness/init.lua` | Evaluated once in the production Lua VM at startup |
| `~/.rness/lua/?.lua` | Available through explicit `require` |
| `~/.rness/lua/?/init.lua` | Available through explicit `require` |
| Explicit `file`, `package`, or inline `config` source | Selected with `rness.plugins.setup`, executed after engine mount with dependencies first |
| Repository `examples/` | Copyable material; not automatically loaded |
| `~/.rness/config.lua` | Not a second automatic startup entry point |

The loader prepends personal module paths to Lua's existing `package.path`; it does not remove the preexisting search paths. Do not treat `require` resolution as confined to `~/.rness`.

## Provider-specific agent profiles

A delegating agent can choose a model on its parent's current connection:

```lua
rness.profiles.declare("small", {
  by_provider = {
    anthropic = {
      model = "claude-haiku-4-5-20251001",
      options = { max_output_tokens = 2048 },
    },
    -- Add exact model IDs for your other configured connections.
  },
})
-- Reference with profile = "small" in rness.agents.declare("scout", ...).
```

Keys are connection names, not protocols. Both spawn and fork resolve the profile before creating the child. The child retains that connection's endpoint/authentication and persists the concrete model/options. Parent reasoning, temperature, and output limits are not carried into the selected variant. Changing the parent later affects future children only. Missing variants fail explicitly without cross-provider fallback. Fixed profiles remain supported; mixing fixed fields and `by_provider` is invalid.

Provider-specific profiles require an existing session selection when selecting an agent or delegating. They cannot serve as a standalone `default_profile` or initial CLI profile without provider context; use a fixed profile or explicit model for the principal.

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
rness.plugins.setup({
  { name = "spinner", file = "./plugins/spinner.lua", watch = true },
  { name = "sessions", file = "./plugins/sessions.lua" },
})
```

Copy the selected files into `~/.rness/plugins/` first. No directory is scanned for automatic activation. With no load declarations, no plugin files run. Removing a declaration disables that plugin on the next restart without deleting its file.

Call `rness.plugins.setup` once with a dense ordered list. Each entry has exactly one
source: `file`, installed `package`, or inline `config` function. File/inline entries
require a name; package identity comes from its manifest. Names accept ASCII letters,
digits, underscores, and hyphens. Relative files resolve against the configuration
directory, not the shell working directory. Optional `enabled=false` skips a source;
`opts` supplies JSON-compatible setup options; `keys` overrides plugin binding slots.
`dependencies = { "questions" }` is a dense list of plugin names. Each dependency
must be explicitly selected and enabled; missing/disabled dependencies and cycles
are rejected. Dependencies load first, even if declared later. A failed dependency
prevents its consumers from executing. Unload consumers before dependencies; an
unload is blocked while loaded dependents remain. Plan's runtime check for an
available Questions frontend still applies in headless operation.
See [plugin lifecycle](../plugins/loading-and-lifecycle.md) for the full contract.

Legacy `rness.plugins.load(name)` remains supported for existing configurations but
cannot be mixed with `plugins.setup`. Migrate all entries together.

A missing selected file fails startup with its path. A Lua execution error is reported as a warning and independent selected plugins are still attempted, but dependents are skipped; plugin execution is not transactional, so earlier side effects are not rolled back.

This is not equivalent to `require`: `require` executes immediately and uses Lua's module cache and search paths. Plugin selection queues a specific file for execution after the engine APIs exist. Plugins cannot call `rness.plugins.load` to add more plugins after declarations close.

### Migration from automatic loading

Add an explicit setup entry per desired source in your chosen order. Restart rness. Merely copying a file into a directory does not activate it. See [plugin lifecycle](../plugins/loading-and-lifecycle.md).

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

The CLI synchronizes loaded Lua tools into the engine registry before firing `ready`. A current frontend session is not guaranteed at that point.

## Applying changes

Restart after editing startup declarations, plugin selection, options, or central mappings. Explicit file/linked-package sources with `watch=true` reload registrations without reexecuting `init.lua`; managed Git packages and inline sources are not watchable. Busy engine maintenance is retried automatically. See [reload guarantees](../plugins/loading-and-lifecycle.md#runtime-reload).

## Troubleshooting

- **Startup declarations are closed:** move the declaration into `init.lua` or a module required by it, then restart.
- **Unknown profile:** check the agent's profile reference and confirm the declaring module is required.
- **Nil session/subagent API during startup:** defer engine calls until after mount.
- **Module not found:** check the file path and use the module name without `.lua`.
- **Examples had no effect:** examples are not loaded automatically. Merge the intended configuration into your personal entry point.

See [known limitations](../../project/known-limitations.md) for unresolved lifecycle and compatibility boundaries.
