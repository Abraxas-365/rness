# Queue and Steer

Ordinary TUI **Enter** queues a followup for a separate turn. **Ctrl+Enter**
steers the current work at the next model-step boundary. Both start a turn when
idle. Steering does not abort an in-flight request or tool. Text and attached
images use the same intent. Slash commands retain their command semantics.
Completion menus and modal previews retain their input precedence.

These are user-input actions only: background job notices and continuable-agent
answers still automatically reach the next step, independently of queued user
messages. This feature does not add queue persistence, editing, or a queue panel.
Pending user inputs remain in memory until consumed; restarting can lose them.
Cancellation/failure parks the pending queue instead of automatically draining it.

## Lua configuration

Add mappings to the existing `rness.keymap.setup` list in `~/.rness/init.lua`:

```lua
rness.keymap.setup({
  { scope = "promptbox", key = "enter", action = "core.promptbox.queue" },
  { scope = "promptbox", key = "ctrl+enter", action = "core.promptbox.steer" },
  -- Portable alternative if your terminal cannot distinguish Ctrl+Enter:
  { scope = "promptbox", key = "f9", action = "core.promptbox.steer" },
})
```

Swap the actions to make Enter steer and Ctrl+Enter queue. The old
`core.promptbox.submit` remains a Queue alias. Mappings are startup configuration;
restart rness after changing them. `/help bindings` shows resolved mappings.

## Lua plugins

Promptbox-scoped actions can call `ctx.promptbox.queue()` or
`ctx.promptbox.steer()` to submit the current draft, including attachments.
They can first call `ctx.promptbox.insert(text)`. Operations run in order after
successful callback completion and retain the existing stale-context/focus guards.
Submission functions expire when the callback returns and are not exposed to
non-promptbox actions.

Outside promptbox actions (hooks, timers, commands), `rness.session.send(id, text)`
queues and `rness.session.steer(id, text)` steers a given session with the same
semantics. See [Lua timers and session delivery](../reference/lua/timer.md).

The default flavor enables `plugins/delivery.lua`: **F8** queues and **F9**
steers. Its `plugin.keys` slots can be remapped or disabled in `rness.plugins.setup`.
Existing installations are not modified automatically; copy
`examples/plugins/delivery.lua` into your plugins directory and add:

```lua
{ name = "delivery", file = "plugins/delivery.lua",
  keys = { queue = "<F8>", steer = "<F9>" } }
```

## Neovim

The local Neovim integration exposes `:RnessQueue [text]` and
`:RnessSteer [text]`. Without text, they send the current line or supplied range.
`:RnessQueuePad` and `:RnessSteerPad` send the scratchpad. Defaults are
`<leader>cQ` for Queue and `<leader>cT` for Steer (line/visual selection).
Existing send bindings retain Queue by default.

```lua
require("rness").setup({
  profile = "nvim",
  send_mode = "queue", -- or "steer" for existing send bindings/scratchpad
  send_keys = { queue = "<leader>cQ", steer = "<leader>cT" }, -- false disables a slot
})
-- Custom bindings can call require('rness').queue(text) or .steer(text).
```

Configure keys at startup. The plugin still rejects overlapping unacknowledged
HTTP sends with "Send pending; please retry"; it permits further sends once the
previous request is acknowledged, even while the agent continues working.
Failed sends preserve staged context and scratchpad text.
