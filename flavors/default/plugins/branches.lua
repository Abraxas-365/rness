-- Add { name = "branches", file = "plugins/branches.lua" } to init.lua's setup list.
-- This app uses legacy keymap/on_key controls, not plugin.keys slots.
-- Branch tree — ctrl+b. Visualize the session DAG around the current
-- session: ancestry up to the root, children fanning out below.
--
-- The branch DATA is engine core (fork refs in the store); this is only
-- the VIEW, over rness.session.{ancestry,children,parent}.
--
-- Keys: j/k move · enter switch to selected · f fork current · esc close

local cursor = 1
local targets = {}  -- row index -> session id

rness.ui.app{
  name = "branches",
  slot = "overlay",
  title = "branches · enter switch · f fork",
  keymap = "ctrl+b",

  view = function(ctx)
    local lines = {}
    targets = {}
    local function add(id, depth, label)
      local marker = (#lines + 1 == cursor) and "> " or "  "
      local here = (id == ctx.session) and " ◀ current" or ""
      lines[#lines + 1] = marker .. string.rep("  ", depth) .. label .. here
      targets[#lines] = id
    end

    -- Root-ward chain: ancestry returns root first, queried session last.
    -- A hop's forked_at is where the NEXT hop branched off (nil on the last).
    local chain = rness.session.ancestry(ctx.session)
    local depth = 0
    for _, hop in ipairs(chain) do
      local label = hop.session
      if hop.forked_at then
        label = label .. "  (fork @" .. hop.forked_at .. ")"
      end
      add(hop.session, depth, label)
      depth = depth + 1
    end

    -- Children of the current session.
    local kids = rness.session.children(ctx.session)
    for _, kid in ipairs(kids) do
      add(kid.session, depth, "⎇ " .. kid.session .. "  (@" .. kid.at .. ")")
    end

    if cursor > #lines then cursor = #lines end
    return lines
  end,

  on_key = function(key, ctx)
    if key == "j" or key == "down" then
      cursor = cursor + 1  -- clamped in view
      return true
    elseif key == "k" or key == "up" then
      cursor = math.max(cursor - 1, 1)
      return true
    elseif key == "enter" then
      local id = targets[cursor]
      if id and id ~= ctx.session then
        return { action = "session:switch", payload = { session = id } }
      end
      return true
    elseif key == "f" then
      local child = rness.session.fork(ctx.session)
      return { action = "session:switch", payload = { session = child } }
    end
    return false
  end,
}
