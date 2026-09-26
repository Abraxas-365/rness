-- User controls for persistent terminals (terminal_* tools): /terminals, an
-- overlay monitor, and tool cards. Everything here reads snapshots; nothing
-- moves the model's read cursor. Add to init.lua's setup list:
--   { name = "terminals", file = "plugins/terminals.lua" }
local usage = "Usage: /terminals [list | <term_id> | stop <term_id> | close <term_id>]"

local function plain(text)
  -- Output is already rendered clean by Rust; still never let escapes reach
  -- the word wrapper or the host terminal.
  return tostring(text or ""):gsub("\27%][^\7\27]*\7", "")
    :gsub("\27%][^\27]*\27\\", ""):gsub("\27%[[0-?]*[ -/]*[@-~]", "")
    :gsub("\27[ -/]*[@-~]", ""):gsub("\r\n", "\n"):gsub("\t", "    ")
    :gsub("[%z\1-\9\11-\31\127]", "")
end

local function shorten(text, chars)
  text = plain(text):gsub("%s+", " "):match("^%s*(.-)%s*$")
  local cut = utf8.offset(text, chars + 1)
  if cut and cut <= #text then text = text:sub(1, cut - 1) .. "…" end
  return text
end

local function duration(secs)
  if not secs then return nil end
  if secs < 60 then return secs .. "s" end
  if secs < 3600 then return math.floor(secs / 60) .. "m" .. (secs % 60 > 0 and (secs % 60 .. "s") or "") end
  return math.floor(secs / 3600) .. "h" .. math.floor(secs % 3600 / 60) .. "m"
end

local function status(t)
  if t.running then
    local s = "running"
    if t.command_secs then s = s .. " " .. duration(t.command_secs) end
    if t.job_id then s = s .. " (" .. t.job_id .. ")" end
    return s
  end
  if t.state:match("^exited") then return t.state end
  if t.last_exit and t.last_exit ~= 0 then return "idle (last exit " .. t.last_exit .. ")" end
  return "idle"
end

local function summary(t)
  local name = t.name ~= t.id and (" " .. t.name) or ""
  local line = t.id .. name .. " [" .. t.shell .. "] " .. status(t)
  if t.command then line = line .. " — " .. shorten(t.command, 80) end
  return line
end

local function hint(t)
  return status(t) .. (t.command and (" · " .. shorten(t.command, 100)) or "")
end

local function choice(value, description)
  if rness.commands.completion_descriptions then
    return { value = value, description = description or "" }
  end
  return value
end

-- ── overlay monitor ────────────────────────────────────────────────────────

local states = {}
local function state(ctx)
  local session = assert(ctx.session, "terminals app requires a session")
  if not states[session] then states[session] = { cursor = 1, offset = 0, follow = true } end
  return states[session]
end

local HELP_LIST = "j/k: select | enter: output | s: stop command | x: close terminal | esc: close"
local HELP_DETAIL = "j/k PgUp/PgDn: scroll | end: follow | s: stop | x: close terminal | esc: list"
local HEIGHT = 24

local function wrapped(text, cols)
  local lines = {}
  for line in (plain(text) .. "\n"):gmatch("(.-)\n") do
    local parts = rness.ui.wrap(line, cols)
    if #parts == 0 then parts = { "" } end
    for _, part in ipairs(parts) do lines[#lines + 1] = part end
  end
  return lines
end

local function list(session)
  local ok, terms = pcall(rness.terminals.list, session)
  return ok and terms or {}
end

local function view(ctx)
  local s = state(ctx)
  local rows = math.max(1, math.min(512, ctx.rows or HEIGHT))
  local cols = math.max(1, math.min(4096, ctx.cols or 80))
  local lines = {}
  if s.selected then
    local ok, t = pcall(rness.terminals.inspect, ctx.session, s.selected, 500)
    if not ok then
      lines[1] = s.selected .. " is closed."
      lines[2] = "esc: list"
    else
      local body = wrapped(t.output ~= "" and t.output or "(no output yet)", cols)
      local page = math.max(1, rows - 3)
      local maximum = math.max(0, #body - page)
      s.offset = s.follow and maximum or math.min(s.offset, maximum)
      lines[1] = shorten(summary(t), cols)
      lines[2] = shorten((t.cwd and ("cwd " .. t.cwd .. " | ") or "") .. "up " .. duration(t.uptime_secs)
        .. " | " .. (s.follow and "FOLLOW" or "PAUSED"), cols)
      lines[3] = HELP_DETAIL
      for i = s.offset + 1, math.min(#body, s.offset + page) do lines[#lines + 1] = body[i] end
      s.page, s.lines = page, #body
    end
  else
    local terms = list(ctx.session)
    s.terms = terms
    for i, t in ipairs(terms) do if t.id == s.cursor_id then s.cursor = i; break end end
    s.cursor = math.max(1, math.min(s.cursor, #terms))
    s.cursor_id = terms[s.cursor] and terms[s.cursor].id
    lines[1] = "Terminals (" .. #terms .. ")"
    if #terms == 0 then lines[2] = "No terminals open in this session." end
    local page = math.max(1, rows - 2)
    local first = math.max(1, s.cursor - page + 1)
    for i = first, math.min(#terms, first + page - 1) do
      lines[#lines + 1] = shorten((i == s.cursor and "> " or "  ") .. summary(terms[i]), cols)
    end
    lines[#lines + 1] = HELP_LIST
  end
  if s.notice then lines[#lines + 1] = "! " .. s.notice end
  while #lines < HEIGHT do lines[#lines + 1] = "" end
  return lines
end

local function notify(ctx, text)
  local message = tostring(text):gsub("^.-: ", "")
  state(ctx).notice = shorten(message, 200)
end

local function on_key(key, ctx)
  local s = state(ctx)
  s.notice = nil
  local target = s.selected or s.cursor_id
  if key == "esc" or key == "q" then
    if s.selected then
      s.selected, s.offset, s.follow = nil, 0, true
      return true
    end
    if key == "esc" then return false end
    return "close"
  end
  if key == "s" and target then
    local ok, err = pcall(rness.terminals.stop, ctx.session, target)
    if not ok then notify(ctx, tostring(err)) end
    return true
  end
  if key == "x" and target then
    local ok, err = pcall(rness.terminals.close, ctx.session, target)
    if not ok then notify(ctx, tostring(err)) end
    s.selected, s.offset, s.follow = nil, 0, true
    return true
  end
  if s.selected then
    local page = s.page or 10
    local delta = ({ k = -1, up = -1, j = 1, down = 1, pageup = -page, pagedown = page })[key]
    if key == "end" then s.follow = true
    elseif key == "home" then s.follow, s.offset = false, 0
    elseif delta then
      s.follow = false
      s.offset = math.max(0, math.min((s.lines or 0) - page, s.offset + delta))
    else return false end
    return true
  end
  local terms = s.terms or {}
  if key == "enter" and terms[s.cursor] then
    s.selected, s.offset, s.follow = terms[s.cursor].id, 0, true
    return true
  end
  local delta = ({ k = -1, up = -1, j = 1, down = 1 })[key]
  if not delta then return false end
  s.cursor = math.max(1, math.min(#terms, s.cursor + delta))
  s.cursor_id = terms[s.cursor] and terms[s.cursor].id
  return true
end

rness.ui.app {
  name = "terminals", slot = "overlay", title = "Terminals",
  refresh_ms = 500, config = { height = HEIGHT },
  capture_escape = true, view = view, on_key = on_key,
}

local function open(ctx, id)
  local s = state(ctx)
  s.selected, s.offset, s.follow = id, 0, true
  return { data = { action = "app:open", app = "terminals", session = ctx.session } }
end

-- ── /terminals ─────────────────────────────────────────────────────────────

rness.commands.register {
  name = "terminals",
  description = "List persistent terminals, watch one, or stop/close it",
  usage = "[list | <term_id> | stop <term_id> | close <term_id>]",
  arguments = { "list", "stop", "close" },
  allow_busy = true,
  complete = function(ctx)
    local terms = list(ctx.session)
    local raw = ctx.raw_input or ""
    local input = raw:match("^%s*(.-)%s*$")
    local choices = {}
    if input == "" then choices[#choices + 1] = choice(raw:gsub("^%s", "", 1), "Open terminals monitor") end
    choices[#choices + 1] = choice("list", "List terminals in this session")
    for _, t in ipairs(terms) do
      local detail = hint(t)
      choices[#choices + 1] = choice(t.id, detail)
      if t.running then choices[#choices + 1] = choice("stop " .. t.id, "Stop: " .. detail) end
      choices[#choices + 1] = choice("close " .. t.id, "Close: " .. detail)
    end
    return choices
  end,
  run = function(ctx)
    local input = ctx.raw_input:match("^%s*(.-)%s*$")
    if input == "" then return open(ctx) end
    if input == "list" then
      local terms = list(ctx.session)
      if #terms == 0 then return { message = "No terminals open in this session." } end
      local lines = { "Terminals in this session:" }
      for _, t in ipairs(terms) do lines[#lines + 1] = summary(t) end
      lines[#lines + 1] = "Watch: /terminals <id>   Stop: /terminals stop <id>   Close: /terminals close <id>"
      return { message = table.concat(lines, "\n"), data = terms }
    end
    local verb, id = input:match("^(%a+)%s+(%S+)$")
    if verb == "stop" or verb == "kill" then
      local stopping = rness.terminals.stop(ctx.session, id)
      return { message = stopping
        and ("Stopping the command in " .. id .. " (Ctrl-C, then TERM/KILL if it keeps running).")
        or (id .. " has no command running.") }
    end
    if verb == "close" then
      rness.terminals.close(ctx.session, id)
      return { message = "Closed " .. id .. "." }
    end
    assert(not input:find("%s"), usage)
    rness.terminals.inspect(ctx.session, input, 0)
    return open(ctx, input)
  end,
}

-- ── tool cards ─────────────────────────────────────────────────────────────

local OUTCOME = {
  done = { "done", "added" }, input = { "waiting for input", "heading" },
  incomplete = { "needs more input", "heading" }, quiet = { "still running", "heading" },
  timeout = { "still running", "heading" }, full_screen = { "full-screen app", "heading" },
  terminal_exited = { "terminal exited", "error" }, cancelled = { "cancelled", "dim" },
  background = { "background", "heading" },
}

local function header(name, right)
  return { left = { { text = name, style = "tool_name" } }, right = right }
end

local TAIL = 12

rness.ui.messagebox.tool_card("terminal_send", function(call)
  local p = call.presentation
  if call.is_error or type(p) ~= "table" or p.kind ~= "terminal" then return nil end
  local label, style
  if p.outcome == "exited" then
    local code = p.exit_code
    label = code == nil and "done" or ("exit " .. code)
    style = (code == nil or code == 0) and "added" or "error"
  else
    local o = OUTCOME[p.outcome] or { tostring(p.outcome), "dim" }
    label, style = o[1], o[2]
  end
  local right = { { text = label, style = style } }
  if p.job_id then right[#right + 1] = { text = " · " .. p.job_id, style = "dim" } end
  if p.elapsed_ms then
    right[#right + 1] = { text = string.format(" · %.1fs", p.elapsed_ms / 1000), style = "dim" }
  end
  local body = { { spans = {
    { text = (p.terminal or "?") .. " $ ", style = "dim" },
    { text = shorten(p.sent or "", 200), style = "heading" },
  } } }
  -- Output minus the trailing [status] line (the header carries it).
  local lines = {}
  for line in (plain(call.output or "") .. "\n"):gmatch("(.-)\n") do lines[#lines + 1] = line end
  while #lines > 0 and (lines[#lines] == "" or lines[#lines]:match("^%[.*%]$")) do lines[#lines] = nil end
  if #lines > TAIL then
    body[#body + 1] = { text = "… " .. (#lines - TAIL) .. " earlier lines", style = "dim" }
  end
  for i = math.max(1, #lines - TAIL + 1), #lines do
    body[#body + 1] = { text = lines[i], style = "tool_output" }
  end
  return { header = header("Terminal", right), body = body }
end)

local function compact(title, describe)
  return function(call)
    if call.is_error or type(call.args) ~= "table" then return nil end
    local text, style = describe(call.args, call.output or "")
    return {
      header = header(title, { { text = text, style = style or "dim" } }),
      body = {},
    }
  end
end

rness.ui.messagebox.tool_card("terminal_open", compact("Terminal open", function(args, output)
  local id = output:match("(term%-%d+)") or "?"
  return id .. (args.name and (" · " .. args.name) or "") .. " · " .. (args.shell or "default shell")
end))
rness.ui.messagebox.tool_card("terminal_close", compact("Terminal close", function(args)
  return args.session_id or "?"
end))
rness.ui.messagebox.tool_card("terminal_signal", compact("Terminal signal", function(args)
  return (args.session_id or "?") .. " · SIG" .. tostring(args.signal or "INT"):upper():gsub("^SIG", "")
end))
