-- Add { name = "sessions", file = "plugins/sessions.lua" } to init.lua's setup list.
-- This app uses legacy keymap/on_key controls, not plugin.keys slots.
-- Session picker — ctrl+s. This project's sessions, most recently active
-- first; jump between them, search, fork.
--
-- Pure Lua over rness.session + rness.ui.app. The "session:switch"
-- action is applied by the host (rehydrates the TUI from the log).
--
-- Keys: ↑/↓ (j/k) move · enter switch · / search · a this project ⇄ all
--       · f fork (and switch to the child) · esc close

local HEIGHT = 22 -- panel rows incl. border; clipped to the terminal
local styles = {
  title = nil, current = "heading", running = "added", time = "dim",
  id = "dim", project = "tool_name", hint = "dim", header = "heading",
  search = "heading", marker = "heading", selected = { bg = "#3c3836" },
}

local state = {
  cursor = 1, top = 1, query = "", searching = false, all = false,
  seen = 0, -- os.time() of the last render: a gap means the picker reopened
}

local function width(text) return utf8.len(text) or #text end
local function truncate(text, n)
  if n <= 0 then return "" end
  if width(text) <= n then return text end
  local cut = utf8.offset(text, n) or (#text + 1)
  return text:sub(1, cut - 1) .. "…"
end
local function basename(path)
  if not path or path == "" then return "?" end
  return path:match("([^/]+)/*$") or path
end

local function ago(ms, now)
  if not ms or ms == 0 then return "" end
  local s = math.max(0, (now - ms) // 1000)
  if s < 60 then return "now" end
  if s < 3600 then return (s // 60) .. "m ago" end
  if s < 86400 then return (s // 3600) .. "h ago" end
  if s < 86400 * 30 then return (s // 86400) .. "d ago" end
  local ok, date = pcall(os.date, "%b %d", ms // 1000)
  return ok and date or ((s // 86400) .. "d ago")
end

local function title_of(id)
  local ok, title = pcall(rness.session.title, id)
  return ok and title or nil
end

-- The sessions to show, current first, then most recently active.
local function recent(opts)
  if rness.session.recent then return rness.session.recent(opts) end
  -- Older binary: no workspace/activity data; newest-created first.
  local ids, out = rness.session.list_roots(), {}
  table.sort(ids, function(a, b) return a > b end)
  for i, id in ipairs(ids) do out[i] = { id = id } end
  return out
end

local function entries(ctx)
  local workspace = rness.session.workspace and rness.session.workspace(ctx.session)
  local scoped = workspace and not state.all
  local list = recent(scoped and { workspace = workspace } or nil)
  local query = state.query:lower()
  local out, current = {}, nil
  for _, s in ipairs(list) do
    if s.id == ctx.session then
      current = s
    elseif query == "" then
      out[#out + 1] = s
    else
      local hay = ((title_of(s.id) or "") .. " " .. s.id .. " " .. basename(s.workspace)):lower()
      if hay:find(query, 1, true) then out[#out + 1] = s end
    end
  end
  if current and query == "" then table.insert(out, 1, current) end
  return out, workspace, scoped
end

local function row(s, ctx, selected, cols, now, scoped)
  local here = s.id == ctx.session
  local running = rness.session.phase(s.id) == "running"
  local badge, badge_style = "  ", nil
  if running then badge, badge_style = "● ", styles.running
  elseif here then badge, badge_style = "◆ ", styles.current end
  local right = {}
  if here then right[#right + 1] = { text = "current  ", style = styles.current } end
  if not scoped then right[#right + 1] = { text = basename(s.workspace) .. "  ", style = styles.project } end
  right[#right + 1] = { text = string.format("%8s", running and "running" or ago(s.updated_ms, now)),
    style = running and styles.running or styles.time }
  right[#right + 1] = { text = "  " .. s.id:sub(-6):lower(), style = styles.id }
  local used = 4
  for _, span in ipairs(right) do used = used + width(span.text) end
  local title = title_of(s.id)
  local untitled = not title or title == ""
  -- Untitled sessions show their full id instead (dimmed).
  if untitled then title = s.id end
  local title_style = here and styles.current or (untitled and styles.id) or styles.title
  return {
    { text = selected and "▸ " or "  ", style = styles.marker },
    { text = badge, style = badge_style },
    { text = truncate(title, cols - used - 2), style = title_style },
    right = right, style = selected and styles.selected or nil,
  }
end

local function view(ctx)
  local now = os.time()
  -- Reopened (no render for a while): start fresh, on the most recent
  -- *other* session so ctrl+s, enter toggles between the last two.
  local reopened = now - state.seen > 2
  if reopened then state.top, state.query, state.searching = 1, "", false end
  state.seen = now
  local list, workspace, scoped = entries(ctx)
  if reopened then state.cursor = (list[1] and list[1].id == ctx.session) and 2 or 1 end
  local rows = math.max(4, math.min(ctx.rows or (HEIGHT - 2), HEIGHT - 2))
  local cols = math.max(20, ctx.cols or 80)
  local page = rows - 3 -- header, blank, footer
  state.cursor = math.max(1, math.min(state.cursor, #list))
  if state.cursor < state.top then state.top = state.cursor end
  if state.cursor >= state.top + page then state.top = state.cursor - page + 1 end
  state.top = math.max(1, math.min(state.top, math.max(1, #list - page + 1)))
  state.list = list

  local lines = {}
  local head = {
    { text = scoped and basename(workspace) or "All projects", style = styles.header },
    { text = "  ·  " .. #list .. (#list == 1 and " session" or " sessions"), style = styles.hint },
  }
  if state.searching or state.query ~= "" then
    head.right = { { text = "/" .. state.query .. (state.searching and "▏" or ""), style = styles.search } }
  elseif workspace then
    head.right = { { text = scoped and "this project" or "all projects", style = styles.hint } }
  end
  lines[#lines + 1] = head
  lines[#lines + 1] = ""
  if #list == 0 then
    lines[#lines + 1] = { text = state.query ~= "" and "  No sessions match." or "  No sessions yet.", style = styles.hint }
  end
  local ms = now * 1000
  for i = state.top, math.min(#list, state.top + page - 1) do
    lines[#lines + 1] = row(list[i], ctx, i == state.cursor, cols, ms, scoped)
  end
  while #lines < rows - 1 do lines[#lines + 1] = "" end
  local more = #list - (state.top + page - 1)
  local hint = state.searching and "type to filter · enter done · esc clear"
    or "↑↓ move · enter open · / search · a " .. (scoped and "all projects" or "this project")
      .. " · f fork · esc close"
  lines[#lines + 1] = { { text = hint, style = styles.hint },
    right = more > 0 and { { text = "↓ " .. more .. " more", style = styles.hint } } or nil }
  return lines
end

local function selected() return state.list and state.list[state.cursor] end

local function on_key(key, ctx)
  if key == " " then key = "space" end
  if state.searching then
    local printable = key == "space" or utf8.len(key) == 1
    if key == "esc" then state.searching, state.query = false, ""
    elseif key == "enter" then state.searching = false
    elseif key == "backspace" then
      local last = utf8.offset(state.query, -1)
      state.query = last and state.query:sub(1, last - 1) or ""
    elseif key == "ctrl+u" then state.query = ""
    elseif printable then state.query = (state.query .. (key == "space" and " " or key)):sub(1, 128)
    elseif key ~= "up" and key ~= "down" then return true end
    if key ~= "up" and key ~= "down" then state.cursor, state.top = 1, 1; return true end
  end
  local n = state.list and #state.list or 0
  local page = math.max(1, ((ctx.rows or HEIGHT - 2) - 3))
  if key == "j" or key == "down" then state.cursor = math.min(state.cursor + 1, n)
  elseif key == "k" or key == "up" then state.cursor = math.max(state.cursor - 1, 1)
  elseif key == "pagedown" then state.cursor = math.min(state.cursor + page, n)
  elseif key == "pageup" then state.cursor = math.max(state.cursor - page, 1)
  elseif key == "home" or key == "g" then state.cursor = 1
  elseif key == "end" or key == "G" then state.cursor = n
  elseif key == "/" then state.searching = true
  elseif key == "a" then state.all, state.cursor, state.top = not state.all, 1, 1
  elseif key == "esc" then
    if state.query ~= "" then state.query, state.cursor, state.top = "", 1, 1; return true end
    return "close"
  elseif key == "enter" then
    local s = selected()
    if s and s.id ~= ctx.session then return { action = "session:switch", payload = { session = s.id } } end
    return "close"
  elseif key == "f" then
    local s = selected()
    if s then return { action = "session:switch", payload = { session = rness.session.fork(s.id) } } end
  else
    return false
  end
  return true
end

rness.ui.app {
  name = "sessions",
  refresh_ms = 500, -- keep external session changes live while visible
  slot = "overlay",
  title = "Sessions",
  keymap = "ctrl+s",
  capture_escape = true, -- esc clears a search before closing
  config = {
    width = 96, height = HEIGHT, title_style = "heading",
    border = { kind = "rounded", style = "overlay_border" },
  },
  view = view,
  on_key = on_key,
}
