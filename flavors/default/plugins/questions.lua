-- Add { name = "questions", file = "plugins/questions.lua" } to init.lua's setup list.
-- Interactive question overlay for AskUser and Plan review.
rness.questions.enable()

-- To customize, replace enable() above with this block and edit the values.
-- These are defaults; omitted fields also use defaults. See docs/reference/lua/commands.md.
-- Editing this repository example does not update your home configuration.
-- Bad configuration rejects the load/reload transactionally, preserving working settings.
--[[
rness.questions.enable {
  enabled = true,
  title = "AskUser", -- nonblank, single-line text
  height = 20, -- terminal rows, 10–65535; clipped to available space
  priority = 99, -- top-level slot priority, below approval by default
  ui = {
    -- width = 80, -- optional centered width in columns, 30–65535; omitted = full width
    padding = { left = 0, right = 0, top = 0, bottom = 0 }, -- each 0–10 cells
    option_spacing = 0, -- blank rows, 0–5
    border = "plain", -- plain | rounded | double | thick | none
    descriptions = "auto", -- auto | always | never
    show_help = true,
    styles = {}, -- empty = active theme; named styles or inline overrides:
    -- styles = { title = "heading", selected = { fg = "#fabd2f", bold = true } },
    symbols = { cursor = "> ", selected = "[x]", unselected = "[ ]", custom = "[+]" },
    labels = {
      other = "Other / write an answer",
      feedback = "Request changes / feedback",
      custom = "Custom: ",
      editing = "Editing > ",
    },
    keys = {}, -- empty = stock bindings; partial overrides keep omitted defaults
    -- keys = { up = "k", down = "j" },
    -- Defaults: up=up, down=down, select=space, submit=enter, next=tab,
    -- previous=shift+tab, dismiss=esc, cancel=ctrl+c, details=pagedown,
    -- page_up=pageup, first=home, last=end. Chords must remain unique.
    -- Printable keys insert text while editing; Enter/Esc remain finish/back fallbacks.
    -- Ctrl+C always cancels and is reserved, even if cancel is remapped.
  },
}
]]
