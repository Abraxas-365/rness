-- tmux context: injects the current tmux session/window/pane info into the
-- model context so it knows where it's running.
--
-- Injected once per turn (step 1 only), re-injected only if state changed.
-- Silent no-op if not running inside tmux. Tagged "tmux"; the default theme
-- hides hook messages (show with `user.sources.hook.tags.tmux = { visible = true }`).
--
-- Format:
--   tmux location (turn N):
--   session "main", window 2 "code", pane 1 (%42)
--   window active, pane active
--
-- Configure via init.lua:
--   rness.tmux_context = {
--     refresh_interval = 0,  -- seconds; 0 = every turn (default)
--   }

local config = rness.tmux_context or {}
local refresh_interval = (config.refresh_interval or 0)

-- Per-session state: { text = "...", time = N }
local state = {}

local function query_tmux()
  -- Check if we're in tmux at all.
  local tmux_pane = os.getenv("TMUX_PANE")
  if not tmux_pane or tmux_pane == "" then return nil end

  -- Query tmux for current location. Use a single command for efficiency.
  local cmd = string.format(
    'tmux display-message -t %q -p '
    .. "'#{session_name}\t#{window_index}\t#{window_name}\t#{pane_index}\t#{pane_id}\t#{window_active}\t#{pane_active}'"
    .. " 2>/dev/null",
    tmux_pane
  )
  local handle = io.popen(cmd)
  if not handle then return nil end
  local output = handle:read("*a")
  handle:close()

  if not output or output == "" then return nil end
  output = output:gsub("%s+$", "")

  -- Parse tab-separated fields.
  local fields = {}
  for field in output:gmatch("[^\t]+") do
    fields[#fields + 1] = field
  end
  if #fields < 7 then return nil end

  local session_name = fields[1]
  local window_index = fields[2]
  local window_name  = fields[3]
  local pane_index   = fields[4]
  local pane_id      = fields[5]
  local window_active = fields[6] == "1"
  local pane_active   = fields[7] == "1"

  local lines = {}
  lines[#lines + 1] = string.format('session "%s", window %s "%s", pane %s (%s)',
    session_name, window_index, window_name, pane_index, pane_id)

  local flags = {}
  if window_active then flags[#flags + 1] = "window active" end
  if pane_active then flags[#flags + 1] = "pane active" end
  if #flags > 0 then
    lines[#lines + 1] = table.concat(flags, ", ")
  end

  return table.concat(lines, "\n")
end

rness.hook.on("pre_step", function(ev, next)
  local decision = next()

  -- Only inject at step 1 of each turn.
  if ev.step ~= 1 then return decision end

  local now = os.time()
  local sid = ev.session
  local s = state[sid]

  -- Throttle: skip if within refresh interval.
  if s and refresh_interval > 0 and (now - (s.time or 0)) < refresh_interval then
    return decision
  end

  -- Query tmux.
  local info = query_tmux()
  if not info then return decision end

  -- Suppress if state hasn't changed.
  if s and s.text == info then return decision end

  local text = string.format("tmux location (turn %d):\n%s", ev.turn, info)

  -- Update state.
  state[sid] = { text = info, time = now }

  -- Merge into decision.
  local kind = (decision and decision.kind) or "enter"
  local messages = (decision and decision.messages) or {}
  if type(messages) == "string" or messages.text then messages = { messages } end
  messages[#messages + 1] = { text = text, tag = "tmux" }
  return { kind = kind, messages = messages }
end)
