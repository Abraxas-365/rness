-- Structured statusline: active model, live status, session ID, and cached
-- context usage. Token usage is loaded the first time a session is rendered
-- (including resumed sessions), then refreshed on turn_end.
local function max_input_tokens(session)
  local selection = rness.session.config(session).selection
  local policies = rness.compaction or {}
  local policy = (selection and policies[selection.route .. "/" .. selection.model]) or policies.default
  return policy and policy.threshold_tokens or 165000
end

local frames = { "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧" }

-- session id → start time; several sessions can run at once
-- (subagents, server clients) — busy means ANY turn is live.
local running = {}

-- session id → what it is doing ("thinking", "writing", a tool name).
-- Frames are ephemeral display state — exactly what this is for.
local doing = {}
local compacting = {}

-- "46.3k/150.0k tok" for the last session that finished a turn.
-- Cached: usage() replays the log, too heavy for a 2/s poll. Recorded
-- usage only moves when a request commits, so turn_end is the right
-- moment to refresh.
local tokens = {}

local function k(n)
  return string.format("%.1fk", n / 1000)
end

local function refresh(session)
  local ok, usage = pcall(rness.session.usage, session)
  if not ok then return end
  tokens[session] = string.format("%s/%s tok", k(usage.input), k(max_input_tokens(session)))
end

rness.hook.on("turn_start", function(ev)
  running[ev.session] = os.time()
end)

rness.hook.on("turn_end", function(ev)
  running[ev.session] = nil
  doing[ev.session] = nil
  compacting[ev.session] = nil
  refresh(ev.session)
end)

-- The live frame stream (same wire shapes SSE clients get).
rness.hook.on("frame", function(f)
  if f.type == "delta" then
    if f.chunk.d == "thinking" then
      doing[f.session] = "thinking"
    elseif f.chunk.d == "text" then
      doing[f.session] = "writing"
    end
    -- tool_args deltas: keep the previous label, the name arrives
    -- with tool_started once the call executes.
  elseif f.type == "tool_started" then
    doing[f.session] = f.name
  elseif f.type == "compaction_started" then
    compacting[f.session] = os.time()
  elseif f.type == "compaction_finished" then
    compacting[f.session] = nil
  elseif f.type == "turn_idle" then
    doing[f.session] = nil
    compacting[f.session] = nil
  end
end)

rness.ui.statusline = {
  style = { fg = "#a89984", bg = "#282828" },
  padding = { left = 1, right = 1 },
  separator = " · ",
  left = function(ctx)
    if ctx.session and tokens[ctx.session] == nil then
      tokens[ctx.session] = ""
      refresh(ctx.session)
    end
    local parts = { { text = ctx.model or "", style = { fg = "#83a598" } } }
    local compact_started = ctx.session and compacting[ctx.session]
    if compact_started then
      local secs = os.time() - compact_started
      parts[#parts + 1] = { text = frames[(secs % #frames) + 1] .. " compacting context", style = { fg = "#fabd2f" } }
      parts[#parts + 1] = { text = tostring(secs) .. "s" }
      parts[#parts + 1] = "(" .. (ctx.session or ""):sub(1, 8) .. ")"
      parts[#parts + 1] = tokens[ctx.session] or ""
      return parts
    end
    local count, oldest, act = 0, nil, nil
    for id, started in pairs(running) do
      count = count + 1
      if oldest == nil or started < oldest then
        oldest, act = started, doing[id]
      end
    end
    if count == 0 then
      parts[#parts + 1] = "idle"
    else
      local secs = os.time() - oldest
      local spin = frames[(secs % #frames) + 1]
      parts[#parts + 1] = { text = spin .. " " .. (act or "working"), style = { fg = "#83a598" } }
      parts[#parts + 1] = { text = tostring(secs) .. "s" }
      parts[#parts + 1] = { text = count > 1 and (count .. " agents") or "" }
    end
    parts[#parts + 1] = "(" .. (ctx.session or ""):sub(1, 8) .. ")"
    parts[#parts + 1] = tokens[ctx.session] or ""
    return parts
  end,
  right = function(ctx)
    return {
      { text = ctx.profile or "" },
      { text = ctx.agent or "" },
    }
  end,
}
