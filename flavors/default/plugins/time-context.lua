-- Time context: injects current time and elapsed duration into the model
-- context at each step so the model can reason about time.
--
-- Injected as a user-role message (intent: inject, source: hook, tag: "time")
-- before each model call. The default theme hides hook messages in the TUI;
-- set `user.sources.hook.tags.time = { visible = true }` to show it.
-- Format:
--   Current time: 2026-09-24T15:30:45-0500 (CDT)
--   Elapsed since last message: 2m 15s
--
-- Configure via init.lua:
--   rness.time_context = {
--     every = "step",        -- "step" (default): before every model call
--                            -- "turn": only on the first step of each turn
--     resend_after = 0,      -- seconds; with every = "turn", also resend on a
--                            -- later step of a long tool loop once this long has
--                            -- passed since the last time message. 0 = never (default)
--     subagents = true,      -- false: never inject into delegated (subagent) sessions
--     refresh_interval = 0,  -- seconds; minimum gap between any two injections.
--                            -- 0 = no throttle (default)
--   }

local config = rness.time_context or {}
local refresh_interval = (config.refresh_interval or 0)
local every = config.every or "step"
if every ~= "step" and every ~= "turn" then
  error("rness.time_context.every must be \"step\" or \"turn\", got " .. tostring(every))
end
local resend_after = config.resend_after or 0
if type(resend_after) ~= "number" or resend_after < 0 then
  error("rness.time_context.resend_after must be a number of seconds >= 0, got " .. tostring(resend_after))
end
local subagents = config.subagents
if subagents == nil then subagents = true end
if type(subagents) ~= "boolean" then
  error("rness.time_context.subagents must be true or false, got " .. tostring(subagents))
end

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

rness.hook.on("pre_step", function(ev, next)
  local decision = next()
  -- Delegated sessions carry `ev.parent` = parent session id. Top-level
  -- sessions carry JSON null (a userdata sentinel in Lua, not nil), so test
  -- for a real id rather than `~= nil`.
  if not subagents and type(ev.parent) == "string" and ev.parent ~= "" then
    return decision
  end
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

  -- Once per turn: tool-loop steps (step > 1) proceed without a time message,
  -- unless resend_after is set and that long has passed since the last one.
  if every == "turn" and ev.step > 1 then
    if resend_after <= 0 or (now - s.last_inject) < resend_after then
      return decision
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
  if type(messages) == "string" or messages.text then messages = { messages } end
  messages[#messages + 1] = { text = text, tag = "time" }
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
