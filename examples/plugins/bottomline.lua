-- Add { name = "bottomline", file = "plugins/bottomline.lua" } to init.lua's
-- single rness.plugins.setup list after copying this file.
-- Alternative to spinner.lua, not a second simultaneous statusline.
-- Displays the latest event's session, not necessarily the selected TUI session.
local active = {}
local latest = nil
local function count_active()
  local count = 0
  for _ in pairs(active) do count = count + 1 end
  return count
end
rness.hook.on("turn_start", function(event)
  active[event.session] = true
  latest = event.session
end)
rness.hook.on("turn_end", function(event)
  active[event.session] = nil
  latest = event.session
end)
rness.ui.statusline(function()
  local label = latest and (" | last session " .. latest) or ""
  return string.format("rness | %d running%s", count_active(), label)
end)
