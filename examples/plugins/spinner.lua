-- spinner.lua — busy indicator + context budget in the statusline.
--
-- Copyable example (zero magic): cp to ~/.rness/plugins/ to use it.
--
-- While a turn runs, shadows the built-in statusline with a spinner,
-- WHAT the agent is doing right now (thinking… / writing… / the tool
-- that is executing), how many agents are working, and elapsed
-- seconds. Once a turn has completed it also shows how much context
-- the session is using against YOUR compact threshold:
-- "46.3k/150.0k tok". Returning nil lets the built-in render — the
-- chain pattern, no configuration.
--
-- Wiring it uses:
--   rness.hook.on("turn_start"/"turn_end")  engine bus → Lua hooks
--   rness.hook.on("frame")                  live frames, protocol wire shape
--   rness.session.usage(id)                 context tokens (replays the log)
--   rness.ui.statusline(fn)                 polled by the host (~2/s)

-- Compact threshold: 80% of the declared context window of the active
-- model (see models.lua) when available, else YOUR literal. Same
-- policy number as autocompact.lua — keep them in sync.
local function max_input_tokens()
  local caps = rness.models.get(rness.model)
  if caps and caps.context_window then
    return math.floor(caps.context_window * 0.8)
  end
  return 150000
end

local frames = { "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧" }
local tick = 0

-- session id → start time; several sessions can run at once
-- (subagents, server clients) — busy means ANY turn is live.
local running = {}

-- session id → what it is doing ("thinking", "writing", a tool name).
-- Frames are ephemeral display state — exactly what this is for.
local doing = {}

-- "46.3k/150.0k tok" for the last session that finished a turn.
-- Cached: usage() replays the log, too heavy for a 2/s poll. Recorded
-- usage only moves when a request commits, so turn_end is the right
-- moment to refresh.
local tokens = nil

local function k(n)
  return string.format("%.1fk", n / 1000)
end

local function refresh(session)
  local ok, usage = pcall(rness.session.usage, session)
  if not ok then return end
  tokens = string.format("%s/%s tok", k(usage.input), k(max_input_tokens()))
end

rness.hook.on("turn_start", function(ev)
  running[ev.session] = os.time()
end)

rness.hook.on("turn_end", function(ev)
  running[ev.session] = nil
  doing[ev.session] = nil
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
  elseif f.type == "turn_idle" then
    doing[f.session] = nil
  end
end)

rness.ui.statusline(function()
  local count, oldest, act = 0, nil, nil
  for id, started in pairs(running) do
    count = count + 1
    if oldest == nil or started < oldest then
      oldest = started
      act = doing[id] -- show what the OLDEST busy session is doing
    end
  end

  if count == 0 then
    return tokens -- idle: budget if known, else built-in renders
  end

  tick = tick + 1
  local spin = frames[(tick % #frames) + 1]
  local secs = os.time() - oldest
  local verb = act and (act .. "…") or "working…"
  local suffix = tokens and (" · " .. tokens) or ""
  if count == 1 then
    return string.format("%s %s %ds%s", spin, verb, secs, suffix)
  end
  return string.format("%s %d agents · %s %ds%s", spin, count, verb, secs, suffix)
end)
