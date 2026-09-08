-- File tree — ctrl+e. The dogfood proof: a stateful, interactive,
-- event-driven app in pure Lua over rness.fs + rness.ui.app.
--
-- Keys: j/k move · enter/l expand dir · h collapse · esc close

local cwd = "."
local expanded = {}  -- path -> true
local cursor = 1
local rows = {}      -- flattened visible rows: { path=, name=, dir=, depth= }

local function is_dir(path)
  -- fs.list errors on non-directories; probe cheaply.
  local ok = pcall(rness.fs.list, path)
  return ok
end

local function build(path, depth)
  local ok, names = pcall(rness.fs.list, path)
  if not ok then return end
  for _, name in ipairs(names) do
    if name ~= ".git" and name ~= "target" and name ~= "node_modules" then
      local full = path .. "/" .. name
      local dir = is_dir(full)
      rows[#rows + 1] = { path = full, name = name, dir = dir, depth = depth }
      if dir and expanded[full] then
        build(full, depth + 1)
      end
    end
  end
end

local function rebuild()
  rows = {}
  build(cwd, 0)
  if cursor > #rows then cursor = #rows end
  if cursor < 1 then cursor = 1 end
end

rness.ui.app{
  name = "tree",
  slot = "sidebar",
  title = "files",
  keymap = "ctrl+e",

  view = function(ctx)
    rebuild()
    -- Window around the cursor (ctx.rows = sidebar viewport height).
    local vis = ctx.rows or 30
    local first = math.max(1, math.min(cursor - math.floor(vis / 2), #rows - vis + 1))
    local last = math.min(#rows, first + vis - 1)
    local lines = {}
    if first > 1 then
      lines[#lines + 1] = "  ↑ " .. (first - 1) .. " more"
      first = first + 1
    end
    if last < #rows then last = last - 1 end
    for i = first, last do
      local row = rows[i]
      local marker = (i == cursor) and "> " or "  "
      local indent = string.rep("  ", row.depth)
      local icon = row.dir and (expanded[row.path] and "▾ " or "▸ ") or "  "
      lines[#lines + 1] = marker .. indent .. icon .. row.name
    end
    if last < #rows then
      lines[#lines + 1] = "  ↓ " .. (#rows - last) .. " more"
    end
    if #rows == 0 then lines[1] = "  (empty)" end
    return lines
  end,

  on_key = function(key)
    if key == "j" or key == "down" then
      cursor = math.min(cursor + 1, #rows)
      return true
    elseif key == "k" or key == "up" then
      cursor = math.max(cursor - 1, 1)
      return true
    elseif key == "enter" or key == "l" then
      local row = rows[cursor]
      if row and row.dir then
        expanded[row.path] = not expanded[row.path] or nil
      end
      return true
    elseif key == "h" then
      local row = rows[cursor]
      if row and row.dir and expanded[row.path] then
        expanded[row.path] = nil
      end
      return true
    end
    return false
  end,
}
