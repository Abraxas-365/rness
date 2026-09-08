# Colorschemes

Register a palette during startup in `~/.rness/init.lua` or a module it requires:

```lua
rness.ui.colorscheme.register("coral", {
  editor_prompt = { fg = "#ff8844", bold = true },
  thinking = { fg = "#928374", italic = true },
  statusline = { fg = "#ffffff", bg = "#602030" },
  statusline_accent = { fg = "#ff8844", bg = "#602030" },
})
rness.ui.colorscheme.set("coral")
```

Restart once to register it. During that TUI execution, use `/colorscheme coral` or `/colorscheme default` to change immediately. Tab completes registered names. This does not reload plugins, send a model request, persist a session event, or rewrite `init.lua`.

## Styles

Groups currently supported:

- `added`, `removed`, `user_prefix`, `assistant_text`, `thinking`
- `tool_name`, `tool_output`, `error`
- `statusline`, `statusline_accent`, `editor_prompt`
- `heading`, `code`, `code_block`, `dim`
- `overlay`, `overlay_border`

Each group accepts `fg`, `bg`, and boolean `bold`, `italic`, `underline`, `reverse` attributes. Colors accept `#RRGGBB`, Ratatui color names such as `red` and `cyan`, and `default` for the terminal default. An explicit false disables an inherited modifier.

Every scheme starts from the stock palette, not the previously selected scheme. Omitted groups and fields keep stock values. Duplicate names, including the reserved name `default`, are rejected. Style groups, colors, and attributes are validated when constructing the TUI palettes; invalid palettes fail TUI startup. Selecting an unknown name interactively reports an error and leaves the current palette unchanged.

## Current boundaries

Lua registration and `set` configure the startup snapshot. Calling them later from a callback does not update the mounted TUI; use the TUI command for live switching. Runtime Lua switching is not implemented. Themes should therefore be required from `init.lua`, not registered in deferred plugins.

There are no dedicated picker groups yet: selection uses `editor_prompt` with reverse video, and other picker text and borders still use default styles. Themes do not change your terminal font, terminal background, or every hard-coded style in every component. These are implementation boundaries, not a promise of complete styling coverage.
