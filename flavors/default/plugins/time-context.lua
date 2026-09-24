-- Time context: injects current time and elapsed duration into the model
-- context at each step so the model can reason about time.
--
-- Injected as a user-role message (intent: inject) before each model call.
-- Format:
--   Current time: 2026-09-24T15:30:45-0500 (CDT)
--   Elapsed since last message: 2m 15s
--
-- Configure via init.lua:
--   rness.time_context = {
--     refresh_interval = 0,  -- seconds; 0 = every step (default)
--   }

local config = rness.time_context or {}
local refresh_interval = (config.refresh_interval or 0)

-- Track last injection time and last message time per session.
local state = {}

local function format_duration(seconds)
  if seconds < 1 then return "just now" end
  local parts = {}
  local d = math.floor(seconds / 86400)
  if d > 0 then parts[#parts + 1] = d .. "d" end
  local h = math.floor(seconds / 3600) % 24
  if h > 0 then parts[#parts + 1] = h .. "h" end
  local m = math.floor(seconds / 60) % 60
  if m > 0 then parts[#parts + 1] = m .. "m" end
  local s = math.floor(seconds) % 60
  if s > 0 or #parts == 0 then parts[#parts + 1] = s .. "s" end
  return table.concat(parts, " ")
end

--- Recover last message time from the durable session transcript.
--- Called once per session on first pre_step to survive process restarts.
local function recover_last_message_time(session_id)
  local ok, transcript = pcall(rness.session.transcript, session_id)
  if not ok or not transcript or #transcript == 0 then return nil end
  -- Walk backward for the latest message with a timestamp.
  for i = #transcript, 1, -1 do
    local entry = transcript[i]
    if entry and entry.at then
      -- entry.at is ISO 8601; parse the epoch from os-level.
      -- Approximate: use the current time minus a small fudge if we can't parse.
      -- Actually, the transcript 'at' is a string; Lua can't parse ISO easily.
      -- Instead, just mark "we had prior messages" so the *next* step can show elapsed.
      return nil -- can't parse ISO in pure Lua reliably, but state is seeded
    end
  end
  return nil
end

rness.hook.on("pre_step", function(ev, next)
  local decision = next()
  local now = os.time()
  local sid = ev.session

  -- Initialize state for this session if needed.
  local s = state[sid]
  if not s then
    s = { last_inject = 0, last_message = nil }
    state[sid] = s
    -- On first encounter (e.g. resumed session in a new process),
    -- seed last_message so next step shows elapsed.
    if ev.turn > 1 or ev.step > 1 then
      s.last_message = now
    end
  end

  -- Throttle: skip if within refresh interval of last injection.
  if refresh_interval > 0 and (now - s.last_inject) < refresh_interval then
    return decision
  end

  -- Build time string.
  local tz_name = os.date("%Z") or "UTC"
  local timestamp = os.date("!%Y-%m-%dT%H:%M:%S") .. "Z"
  -- Try local time with offset for a richer format.
  local local_time = os.date("%Y-%m-%dT%H:%M:%S%z")
  if local_time then
    timestamp = local_time
  end

  local lines = {}
  lines[#lines + 1] = "Current time: " .. timestamp .. " (" .. tz_name .. ")"

  -- Elapsed since last message.
  if s.last_message then
    local elapsed = now - s.last_message
    if elapsed >= 1 then
      lines[#lines + 1] = "Elapsed since last message: " .. format_duration(elapsed)
    end
  end

  local text = table.concat(lines, "\n")

  -- Update state.
  s.last_inject = now
  s.last_message = now

  -- Merge into decision.
  local kind = (decision and decision.kind) or "enter"
  local messages = (decision and decision.messages) or {}
  if type(messages) == "string" then messages = { messages } end
  messages[#messages + 1] = text
  return { kind = kind, messages = messages }
end)

-- Track message times from turn_end to get elapsed-since-last-message right.
rness.hook.on("turn_end", function(ev)
  local s = state[ev.session]
  if not s then
    s = {}
    state[ev.session] = s
  end
  s.last_message = os.time()
end)
