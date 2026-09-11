-- diffcards.lua — rich cards for edit/write tool calls.
--
-- Copy this file, then add { name = "diffcards", file = "plugins/diffcards.lua" }
-- to init.lua's single rness.plugins.setup list. Copying alone does not enable it.
--
-- rness.ui.tool_card(name, render) registers a renderer for one tool
-- ("*" = catch-all). render(call) gets { name=, args=, output=,
-- is_error= } and returns lines — a string, or { text=, style= } with a
-- THEME STYLE NAME ("added", "removed", "title", "dim", "error",
-- "heading", "code"). Return nil to fall back to the built-in card.
--
-- When a card is returned it owns the WHOLE render: rness suppresses
-- the built-in "⚙ Name" header too, so the first line here is the
-- header — tool name and file path on one line.

local MAX = 12 -- diff lines per side before truncating

-- Absolute path for display. Tool args carry the path as the model
-- wrote it (often relative); resolve against the process cwd.
local cwd = os.getenv("PWD")
local function abspath(p)
  if p:sub(1, 1) == "/" or p:sub(1, 1) == "~" or not cwd then return p end
  return cwd .. "/" .. p
end

local function push_block(lines, text, prefix, style, cap)
  local n = 0
  for line in (text .. "\n"):gmatch("(.-)\n") do
    n = n + 1
    if n > cap then
      lines[#lines + 1] = { text = "    … truncated", style = "dim" }
      return
    end
    lines[#lines + 1] = { text = "  " .. prefix .. line, style = style }
  end
end

-- Output block for cards that just echo the tool output (Read/Bash).
local function push_output(lines, output, cap)
  local n = 0
  for line in (output .. "\n"):gmatch("(.-)\n") do
    n = n + 1
    if n > cap then
      lines[#lines + 1] = { text = "  │ … truncated", style = "dim" }
      return
    end
    lines[#lines + 1] = { text = "  │ " .. line, style = "" }
  end
end

rness.ui.tool_card("Read", function(call)
  if call.is_error then return nil end
  local a = call.args
  if type(a) ~= "table" or not a.path then return nil end
  local lines = {
    { text = "⚙ Read  📄 " .. abspath(a.path), style = "title" },
  }
  push_output(lines, call.output or "", 6)
  return lines
end)

rness.ui.tool_card("Bash", function(call)
  if call.is_error then return nil end
  local a = call.args
  if type(a) ~= "table" or not a.command then return nil end
  local cmd = a.command:gsub("\n", " ⏎ ")
  local lines = {
    { text = "⚙ Bash  $ " .. cmd, style = "title" },
  }
  push_output(lines, call.output or "", 8)
  return lines
end)

rness.ui.tool_card("Edit", function(call)
  if call.is_error then return nil end -- errors: built-in card is clearer
  local a = call.args
  if type(a) ~= "table" or not a.path then return nil end
  local lines = {
    { text = "⚙ Edit  ✎ " .. abspath(a.path), style = "title" },
  }
  push_block(lines, a.old_string or "", "- ", "removed", MAX)
  push_block(lines, a.new_string or "", "+ ", "added", MAX)
  return lines
end)

rness.ui.tool_card("Write", function(call)
  if call.is_error then return nil end
  local a = call.args
  if type(a) ~= "table" or not a.path then return nil end
  local lines = {
    { text = "⚙ Write  ✚ " .. abspath(a.path), style = "title" },
  }
  push_block(lines, a.content or "", "+ ", "added", MAX)
  return lines
end)
