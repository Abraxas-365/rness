-- tmux context: injects the current tmux session/window/pane and the
-- window's pane layout into the model context so it knows where it runs.
--
-- Injected once per turn (step 1 only), re-injected only if state changed
-- (moved, renamed, or re-laid-out pane). Silent no-op if not genuinely
-- inside tmux. Tagged "tmux"; the default theme hides hook messages (show
-- with `user.sources.hook.tags.tmux = { visible = true }`).
--
-- "Genuinely": `$TMUX_PANE` alone is not trusted. A terminal started from a
-- tmux shell (VS Code's integrated terminal, a desktop launcher) inherits
-- `$TMUX`/`$TMUX_PANE` without living in that pane. The pane's `pane_tty`
-- must equal this process's controlling terminal (dsh tmux-context parity).
--
-- Format:
--   tmux location (turn N):
--   session "main", window 2 "code", pane 1 (%42)
--   window active, pane active
--   layout 5aed,176x79,0,0{88x79,0,0,42,87x79,89,0,43}
--
-- Configure via init.lua:
--   rness.tmux_context = {
--     refresh_interval = 0,  -- seconds; 0 = every turn (default)
--   }

local config = rness.tmux_context or {}
local refresh_interval = (config.refresh_interval or 0)
if type(refresh_interval) ~= "number" or refresh_interval < 0 then
  error("rness.tmux_context.refresh_interval must be a non-negative number of seconds")
end

-- Per-session state: { text = "...", time = N }
local state = {}

local FIELDS = {
  "#{session_name}", "#{window_index}", "#{window_name}", "#{pane_index}",
  "#{pane_id}", "#{window_active}", "#{pane_active}", "#{window_layout}",
}

-- One shell: bail unless the pane named by $TMUX_PANE owns our controlling
-- tty, then print the fields. The popen shell inherits rness's controlling
-- terminal, so `ps -p $$` names it (`ttys003` / `pts/3`; pane_tty carries
-- the `/dev/` prefix).
local QUERY = table.concat({
  '[ -n "$TMUX_PANE" ] || exit 1',
  "self_tty=$(ps -o tty= -p $$ 2>/dev/null | tr -d ' ')",
  'case "$self_tty" in ""|"?"|"??") exit 1;; esac',
  [[pane_tty=$(tmux display-message -t "$TMUX_PANE" -p '#{pane_tty}' 2>/dev/null) || exit 1]],
  '[ "$pane_tty" = "/dev/$self_tty" ] || exit 1',
  [[exec tmux display-message -t "$TMUX_PANE" -p ']] .. table.concat(FIELDS, "\t") .. [[' 2>/dev/null]],
}, "\n")

-- Split on tabs keeping empty fields (a window name can be empty).
local function split_tabs(s)
  local out, start = {}, 1
  while true do
    local i = s:find("\t", start, true)
    if not i then
      out[#out + 1] = s:sub(start)
      return out
    end
    out[#out + 1] = s:sub(start, i - 1)
    start = i + 1
  end
end

local function query_tmux()
  local tmux_pane = os.getenv("TMUX_PANE")
  if not tmux_pane or tmux_pane == "" then return nil end

  local handle = io.popen(QUERY)
  if not handle then return nil end
  local output = handle:read("*a")
  handle:close()
  if not output or output == "" then return nil end

  local fields = split_tabs((output:match("^[^\n]*")))
  if #fields ~= #FIELDS then return nil end
  local session_name, window_index, window_name, pane_index,
        pane_id, window_active, pane_active, window_layout = table.unpack(fields)
  if pane_id == "" then return nil end

  local lines = {}
  lines[#lines + 1] = string.format('session "%s", window %s "%s", pane %s (%s)',
    session_name, window_index, window_name, pane_index, pane_id)

  local flags = {}
  if window_active == "1" then flags[#flags + 1] = "window active" end
  if pane_active == "1" then flags[#flags + 1] = "pane active" end
  if #flags > 0 then
    lines[#lines + 1] = table.concat(flags, ", ")
  end
  if window_layout ~= "" then
    lines[#lines + 1] = "layout " .. window_layout
  end

  return table.concat(lines, "\n")
end

rness.hook.on("pre_step", function(ev, next)
  local decision = next()

  -- Only inject at step 1 of each turn.
  if ev.step ~= 1 then return decision end
  if decision and decision.kind == "reject" then return decision end

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
