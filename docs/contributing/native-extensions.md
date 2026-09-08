# Native and Lua extension parity

This is an implementation inventory, not a claim that every Lua callback has an interchangeable native plugin registry.

| Surface | Native mechanism | Lua path | Current boundary |
| --- | --- | --- | --- |
| Tools | Engine `Tool` trait and `ToolRegistry` | `LuaTool` adapter | Shared dispatch and approvals; `try_register` rejects duplicates and `replace` requires an existing name |
| Sessions | `SessionService` | `rness.session` bridge | Native service owns durability and validation |
| Delegation | `SubagentRuntime` | `rness.subagents` bridge | Shared authority and lineage checks |
| Statusline | Kernel `TextProvider`, TUI `StatusText::refresh` | `LuaHost` implements `TextProvider` | Shared async text contract; stock CLI chooses the Lua adapter |
| Hooks | Kernel `HookSink` plus typed event bus | `LuaHost` implements the sink | Shared nonblocking event submission; host retains subscription lifetime |
| Tool cards | Engine `ToolCards`, kernel `StyledLine` | `LuaHost` adapter | Same lines reach the TUI cache without mirrored types |
| UI applications | Kernel `Applications`, `AppSpec`, `AppKeyOutcome` | `LuaHost` adapter | Shared roster/view/key contracts; host still owns focus and mounting |
| Keybindings | TUI keymap state | Lua declarations collected by host | Composition controls application of bindings |
| Providers | Engine provider trait | Startup connection declarations | Lua connection configuration is not a custom transport implementation |

## First shared visual contract

`rness_kernel::presentation::TextProvider` is language-independent and domain-neutral:

```rust
#[async_trait::async_trait]
impl rness_kernel::presentation::TextProvider for MyExtension {
    async fn text(&self) -> Option<String> {
        Some("custom status".into())
    }
}
```

`None` declines rendering and restores frontend fallback behavior. The callback runs outside the synchronous render loop. The host owns polling and task lifetime; the trait does not spawn background tasks or prescribe a refresh interval.

The CLI's existing statusline poll now uses `StatusText::refresh` with the Lua host through this contract. A custom composition can supply a native implementation at the same call site. This does not introduce a provider competition registry or an automatic replacement policy.

## Runnable Rust example

```sh
cargo run -p rness-cli --example native_extension
```

The [example source](../../crates/rness-cli/examples/native_extension.rs) registers a native tool and publishes native status text using the shared contract. It runs without model credentials. It is a composition demonstration, not a complete terminal application: the example does not start a TUI or modify the stock binary's runtime registrations.

To integrate native behavior into a custom terminal composition, mount `ext_statusline::install`, retain the returned handle, and refresh it from your existing host task. Register tools through the engine registry and dispatch model calls through the normal engine path rather than directly executing them.

## Limits and next work

Native crates are compiled into a binary; `.so`/`.dylib` loading is not provided. Persistence and permission enforcement are not replaceable presentation callbacks.

The shared hook contract submits an event name and JSON payload without waiting for completion. It is notification-only, not a veto or waterfall contract. Native implementations must enqueue slow work themselves. Applications return explicit key outcomes and view errors; tool cards return `None` to decline rendering.

Tool registration now distinguishes `try_register` (error on duplicate) and `replace` (error on missing name, returns the previous implementation). The convenience `register` panics on duplicates and is suitable only for trusted, statically composed unique registrations. Lua bridge collisions are warned and skipped rather than replacing native tools implicitly. Lua synchronization retains the exact installed `Arc<dyn Tool>` handles rather than names alone. `replace_if_current` and `unregister_if_current` compare registration identity under the registry write lock: a stale host cannot overwrite or remove an implementation installed by another owner. Initial CLI synchronization happens once and passes those handles to the watcher. This ownership boundary is host-level, not yet per Lua source file. An old Lua adapter still invokes the retained host VM; registration identity does not freeze the Lua callback across a VM reload.

## Explicit Lua application and card replacement

`rness.ui.app(spec)` and `rness.ui.tool_card(name, callback)` reject duplicate names. Override a previous declaration explicitly, in a later loaded plugin:

```lua
rness.ui.replace_app {
  name = "existing-panel",
  slot = "overlay",
  title = "Custom panel",
  view = function(ctx) return { "Custom content" } end,
}
rness.ui.replace_tool_card("existing-tool", function(call)
  return { { text = call.output, style = "dim" } }
end)
```

The named Lua app/card must already exist, otherwise replacement fails. An app replacement is complete: omitted `on_key` removes the previous handler, and omitted metadata uses normal defaults. Names must be nonempty, app slots must be `sidebar` or `overlay`, and callback/metadata types are checked before enqueueing. Replaced registry keys are released during registration draining.

These are load-time operations, not runtime replacement APIs. Restart production to apply edited implementations. Wildcard card `"*"` is its own registration; replacing it does not replace exact-name renderers. Named Lua chunks have ownership and coordinated unload through the local TUI. See [plugin lifecycle](../guides/plugins/loading-and-lifecycle.md) for teardown, replacement ownership, and generation-aware frontend cleanup. This does not establish a universal native-plugin unload host.

## Lua hook subscription lifetime

`rness.hook.on(event, callback)` returns an unsubscribe function scoped to that subscription:

```lua
local unsubscribe = rness.hook.on("turn_end", function(event)
  rness.log.info("finished: " .. event.session)
end)
-- During plugin-controlled teardown:
-- unsubscribe() -- true once, false on subsequent calls
```

Ignoring the return value preserves a persistent subscription. Unsubscribing releases the callback and removes its registration; it does not remove another listener with the same event name. Dispatch snapshots registration order: listeners added during dispatch start on the next emission, and listeners removed before their turn are skipped. Self-unsubscription is supported for both host events and `rness.events.emit`.

This is explicit subscription cleanup, not automatic per-file unload. Plugins still need a future owner lifecycle to invoke their cleanup collectively. Native typed kernel events retain their existing disposer interface; `HookSink` only describes event submission.

## Explicit Lua tool replacement

During plugin loading, `rness.tool.register { ... }` rejects a duplicate Lua tool name. Use `rness.tool.replace { ... }` to deliberately replace a tool already declared in the same VM. Replacement is a complete declaration, not a partial patch; use `schema` for the JSON Schema field. Unknown names fail, and the previous registry key is released when the replacement is drained.

```lua
rness.tool.register {
  name = "greeting",
  run = function() return "hello" end,
}
rness.tool.replace {
  name = "greeting",
  description = "Return a customized greeting",
  run = function() return "hello from my plugin" end,
}
```

This is not a mechanism to replace Rust tools from Lua: the VM cannot claim native registrations. Load the original Lua plugin before the replacement plugin. Registrations drain during chunk loading; runtime callback mutation is not a supported replacement lifecycle. Production changes still require restart. Ownership inside the VM is not yet isolated by plugin file, and plugin execution is not transactional.

Full public extension parity remains unfinished: a unified lifecycle/ownership registry for apps, cards, and hooks, Lua-to-native replacement authorization, and fully injectable frontend composition still require work. Shared contracts are not an automatic native plugin loader.
