# Lua plugin packages

Packages are trusted Lua code, not sandboxed applications. Installation does not execute Lua, run build scripts, or change `init.lua`.

## Commands

```sh
rness plugin install https://github.com/owner/plugin --rev v1.0.0
rness plugin link ./plugin
rness plugin list
rness plugin update plugin-name --rev v1.1.0
rness plugin remove plugin-name --confirm
```

Git installation requires HTTPS and an explicit revision. The inventory stores the resolved commit. Git hooks, user Git configuration, submodules, and repository symlinks are disabled or rejected. Private repositories requiring interactive credentials are not supported.

A linked directory is mutable and is never deleted by removal. Removing a Git package deletes its current managed checkout. Updates delete the immediately superseded managed checkout after publication. Orphans from earlier versions or interrupted operations are not scanned or deleted automatically. Remove its activation from `init.lua` as well; the installer does not evaluate or edit arbitrary Lua configuration.

## Package structure

Create `rness-plugin.json`:

```json
{"name":"greeting","entrypoint":"plugin.lua","api_version":1}
```

Create `plugin.lua`:

```lua
rness.tool.register {
  name = "greeting",
  description = "Return a greeting",
  schema = { type = "object" },
  run = function()
    return require("greeting.message")
  end,
}
```

Create `lua/greeting/message.lua`:

```lua
return "Hello"
```

Then link the directory and explicitly activate it:

```lua
rness.plugins.setup({
  { package = "greeting", watch = true }, -- watch only linked development packages
})
```

Restart rness to load it. Omit `watch` for managed Git packages (watching them is rejected). Add this entry to your existing single setup list rather than calling setup again. Package entries must omit `name`; identity comes from the manifest. `/unload greeting` unloads the running extension; it does not uninstall the package. Stop affected sessions before updating or removing managed packages.

## Module resolution

The loader snapshots package Lua helpers when discovering the package. Its entrypoint and helpers share a private `require` cache, available during callbacks as well as initial loading. Package helpers take precedence over global modules; unresolved names fall back to ordinary `require`. Circular private imports fail explicitly. Both `lua/name.lua` and `lua/name/init.lua` resolve to `name`; declaring both is rejected. Module symlinks are rejected.

Updating or deleting package files does not replace these already captured helper sources in an existing VM. Arbitrary plugin filesystem reads are not covered by this guarantee.

## Publication and failure behavior

Writers serialize through `packages/.writer`. A crash can leave that file behind; investigate the owning process before removing it. Installation validates in temporary storage, then atomically publishes `packages/lock.json`. Failed publication drops the staged checkout, leaving the prior inventory intact. Cleanup failure after a successful update is reported explicitly as a committed update with an old checkout left behind.

Removal validates that a Git checkout is directly inside managed storage, renames it temporarily, and publishes the new inventory. Publication failure restores the checkout. Filesystem failure during final cleanup can leave garbage after deregistration.

This is not a crash-consistent database or a dependency resolver. Compatibility is checked against manifest API version 1, not a guarantee that every future API change is compatible. Tests exercise real Git fetch and checkout against temporary local repositories, commit pinning, missing revisions, update cleanup, and injected publication failures during update and removal. Only the transport endpoint is substituted in those tests; public HTTPS connectivity, TLS, and authentication are not exercised.
