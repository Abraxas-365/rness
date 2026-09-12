local p = {
  bg = "#282828", card = "#3c3836", fg = "#ebdbb2", muted = "#a89984",
  border = "#665c54", yellow = "#fabd2f", green = "#b8bb26",
  red = "#fb4934", blue = "#83a598", purple = "#d3869b",
}
rness.ui.colorscheme.register("gruvbox", {
  assistant_text = { fg = p.fg, bg = p.bg },
  user_prefix = { fg = p.yellow, bold = true },
  thinking = { fg = p.muted, italic = true },
  tool_name = { fg = p.blue, bold = true },
  tool_output = { fg = p.fg, bg = p.card },
  error = { fg = p.red, bold = true },
  added = { fg = p.green }, removed = { fg = p.red },
  heading = { fg = p.yellow, bold = true },
  code = { fg = p.green }, code_block = { fg = p.fg, bg = p.card },
  dim = { fg = p.muted },
  editor_prompt = { fg = p.yellow, bg = p.card, bold = true },
  statusline = { fg = p.muted, bg = p.card },
  statusline_accent = { fg = p.yellow, bg = p.card, bold = true },
  overlay = { fg = p.fg, bg = p.bg },
  overlay_border = { fg = p.border, bg = p.bg },
})
rness.ui.colorscheme.set("gruvbox")
rness.ui.messagebox = {
  padding = { left = 1, right = 1 },
  user = {
    style = { fg = p.fg, bg = p.card }, padding = { left = 1, right = 1 },
    marker = false, label = { text = "You", style = "user_prefix" },
  },
  assistant = { style = "assistant_text", marker = false },
  thinking = { display = "preview", preview_lines = 3, style = "thinking" },
  tool = {
    style = { fg = p.fg, bg = p.card },
    border = { kind = "rounded", style = { fg = p.border } },
    padding = { left = 1, right = 1 }, display = "preview", preview_lines = 8,
    header = { show_name = true, show_status = true, show_duration = true },
    states = { error = { display = "expanded", style = "error" } },
  },
  error = { style = "error", display = "expanded" },
  tools = {
    Edit = { render = function(call)
      if call.is_error then return nil end
      local a = call.args
      if type(a) ~= "table" then return nil end
      local lines = { { text = "Edit  " .. (a.path or ""), style = "tool_name" } }
      for _, side in ipairs({ { a.old_string, "- ", "removed" }, { a.new_string, "+ ", "added" } }) do
        local count = 0
        for line in ((side[1] or "") .. "\n"):gmatch("(.-)\n") do
          count = count + 1
          if count > 12 then
            lines[#lines + 1] = { text = "... more lines", style = "dim" }
            break
          end
          lines[#lines + 1] = { text = side[2] .. line, style = side[3] }
        end
      end
      return lines
    end },
  },
}
-- This styles supported surfaces; it does not alter the terminal palette/font.
