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
  selection = {
    style = { bg = "#504945" },
    marker = { text = "▎", style = { fg = p.blue, bold = true } },
  },
  padding = { left = 1, right = 1 },
  user = {
    style = { fg = p.fg, bg = p.card }, padding = { left = 1, right = 1 },
    marker = false, label = { text = "You", style = "user_prefix" },
  },
  assistant = { style = "assistant_text", marker = false },
  thinking = { display = "preview", preview_lines = 3, style = "thinking" },
  compaction = {
    style = { fg = p.fg, bg = p.card },
    border = { kind = "rounded", style = { fg = p.border } },
    padding = { left = 1, right = 1 }, display = "preview", preview_lines = 6,
    header = { style = "tool_name" },
  },
  tool = {
    style = { fg = p.fg, bg = p.card },
    border = { kind = "rounded", style = { fg = p.border } },
    padding = { left = 1, right = 1 }, display = "preview", preview_lines = 8,
    header = { show_name = true, show_status = true, show_duration = true },
    states = { error = { display = "expanded", style = "error" } },
  },
  error = { style = "error", display = "expanded" },
  tools = {
    subagent = { display = "collapsed", render = function(call)
      local p = call.presentation
      if not p or p.kind ~= "subagent_activity" then return nil end
      local a = call.args or {}
      local body = { { text = a.prompt or "", style = "dim" } }
      for _, line in ipairs(p.lines or {}) do
        body[#body + 1] = { text = line, style = "tool_output" }
      end
      local streams = {}
      for id in pairs(p.streams or {}) do streams[#streams + 1] = id end
      table.sort(streams)
      for _, id in ipairs(streams) do
        body[#body + 1] = { text = id .. " (live)", style = "dim" }
        body[#body + 1] = { text = p.streams[id], style = "tool_output" }
      end
      if p.live and p.live ~= "" then body[#body + 1] = { text = p.live, style = "assistant_text" } end
      return {
        header = { text = string.format("%s · %s · %ds · %s",
          a.agent or "subagent", p.status, math.floor(p.elapsed_ms / 1000), p.activity), style = "tool_name" },
        body = body,
      }
    end },
    Bash = { render = function(call)
      local args = type(call.args) == "table" and call.args or {}
      local command = type(args.command) == "string" and args.command or ""
      local duration = call.presentation and tonumber(call.presentation.duration_ms)
      local status = call.is_error and "failed" or "done"
      local status_style = call.is_error and "error" or { fg = p.green, bold = true }
      local header = {
        left = {
          { text = "Bash", style = "tool_name" },
          { text = " · ", style = "dim" },
          { text = status, style = status_style },
        },
        right = duration and { { text = string.format("%.0f ms", duration), style = "dim" } } or {},
      }
      local body = {
        { kind = "code", text = command, language = "bash", syntax_highlight = true, line_numbers = false },
      }
      if type(args.workdir) == "string" and args.workdir ~= "" then
        body[#body + 1] = { spans = {
          { text = "in ", style = "dim" },
          { text = args.workdir, style = "code" },
        } }
      end
      if type(call.output) == "string" and call.output ~= "" then
        body[#body + 1] = { text = call.output, style = "tool_output" }
      end
      return { header = header, body = body }
    end },
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
    Write = { render = function(call)
      if call.is_error then return nil end
      local a = call.args
      if type(a) ~= "table" or type(a.path) ~= "string" or type(a.content) ~= "string" then return nil end
      return {
        header = {
          left = {
            { text = "Write", style = "tool_name" },
            { text = "  " .. a.path, style = "code" },
          },
          right = { { text = "done", style = { fg = p.green, bold = true } } },
        },
        body = {
          { kind = "code", text = a.content, syntax_highlight = true, line_numbers = true },
        },
      }
    end },
  },
}
-- This styles supported surfaces; it does not alter the terminal palette/font.
