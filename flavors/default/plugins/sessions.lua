-- Add { name = "sessions", file = "plugins/sessions.lua" } to init.lua's setup list.
-- This app uses legacy keymap/on_key controls, not plugin.keys slots.
-- Session picker — ctrl+s. List sessions, jump between them, fork.
--
-- Pure Lua over rness.session + rness.ui.app. The "session:switch"
-- action is applied by the host (rehydrates the TUI from the log).
--
-- Keys: j/k move · enter switch · f fork (and switch to the child) · esc close

local cursor = 1
local ids = {}

local function refresh()
  ids = rness.session.list()
  -- newest first (ulids sort lexicographically)
  table.sort(ids, function(a, b) return a > b end)
  if cursor > #ids then cursor = #ids end
  if cursor < 1 then cursor = 1 end
end

rness.ui.app{
  name = "sessions",
  refresh_ms = 500, -- keep external session changes live while visible
  slot = "overlay",
  title = "sessions · enter switch · f fork",
  keymap = "ctrl+s",

  view = function(ctx)
    refresh()
    -- Window around the cursor: the host tells us how many rows the
    -- overlay can show (ctx.rows; nil before the first render).
    local rows = ctx.rows or 20
    local first = math.max(1, math.min(cursor - math.floor(rows / 2), #ids - rows + 1))
    local last = math.min(#ids, first + rows - 1)
    local lines = {}
    if first > 1 then
      lines[#lines + 1] = "  ↑ " .. (first - 1) .. " more"
      first = first + 1
    end
    if last < #ids then last = last - 1 end
    for i = first, last do
      local id = ids[i]
      local marker = (i == cursor) and "> " or "  "
      local here = (id == ctx.session) and " (current)" or ""
      local phase = rness.session.phase(id)
      local badge = (phase == "running") and " ●" or ""
      local title = rness.session.title(id)
      local label = title and (title .. "  " .. id) or id
      lines[#lines + 1] = marker .. label .. here .. badge
    end
    if last < #ids then
      lines[#lines + 1] = "  ↓ " .. (#ids - last) .. " more"
    end
    if #ids == 0 then lines[1] = "  no sessions" end
    return lines
  end,

  on_key = function(key, ctx)
    if key == "j" or key == "down" then
      cursor = math.min(cursor + 1, #ids)
      return true
    elseif key == "k" or key == "up" then
      cursor = math.max(cursor - 1, 1)
      return true
    elseif key == "enter" then
      local id = ids[cursor]
      if id then
        return { action = "session:switch", payload = { session = id } }
      end
      return true
    elseif key == "f" then
      local id = ids[cursor]
      if id then
        local child = rness.session.fork(id)
        return { action = "session:switch", payload = { session = child } }
      end
      return true
    end
    return false
  end,
}
