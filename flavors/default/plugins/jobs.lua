-- User-only job inspection. These snapshots never consume the model's output
-- cursor, and commands remain available while the current turn is busy.
local usage = "Usage: /jobs [list | <job_id> | stop <job_id>]"

local function summary(job)
  local status = job.cancellation_requested and job.running and "stopping" or job.status
  if job.exit_code ~= nil then status = status .. " (code " .. job.exit_code .. ")" end
  return job.job_id .. " [" .. job.kind .. "] " .. status .. " — " .. job.label
end

local function hint(job)
  local status = job.cancellation_requested and job.running and "stopping" or job.status
  if job.exit_code ~= nil then status = status .. " (code " .. job.exit_code .. ")" end
  local label = job.label:gsub("%s+", " "):match("^%s*(.-)%s*$")
  -- Bound the preview by Unicode characters, not bytes.
  local cut = utf8.offset(label, 121)
  if cut and cut <= #label then label = label:sub(1, cut - 1) .. "…" end
  return job.kind .. " · " .. status .. (label ~= "" and (" · " .. label) or "")
end

local function active_jobs(session)
  local active = {}
  for _, job in ipairs(rness.jobs.list(session)) do
    -- Cancellation is still active until the underlying process settles.
    if job.running then active[#active + 1] = job end
  end
  return active
end

local function choice(value, description)
  -- Installed plugins may be loaded by an older binary.
  if rness.commands.completion_descriptions then
    return { value = value, description = description or "" }
  end
  return value
end

-- Everything above the host's plain-string app contract lives here. State is
-- private to this plugin generation and scoped to the command's session.
local states = {}
local defaults = {
  title = "Background jobs", refresh_ms = 250,
  layout = { height = 24 },
  keys = {
    up = { "k", "up" }, down = { "j", "down" },
    page_up = { "pageup" }, page_down = { "pagedown" },
    home = { "home" }, follow = { "end" }, open = { "enter" },
    back = { "esc" }, pause = { "space" },
  },
  text = {
    list = "Background jobs", empty = "No background jobs in this session.",
    list_help = "j/k: select | enter: output | esc: close",
    detail_help = "j/k PgUp/PgDn: scroll | space: pause/follow | end: follow | esc: list",
    following = "FOLLOW", paused = "PAUSED", output = "Recent output (non-consuming, up to 8 KiB)",
    no_output = "(no output yet)", unavailable = "Job no longer available", marker = "> ",
  },
}
local options
local function merge(base, override)
  local result = {}
  for k, v in pairs(base) do result[k] = type(v) == "table" and merge(v, {}) or v end
  for k, v in pairs(override) do
    if type(v) == "table" and type(base[k]) == "table" and k ~= "keys" then
      result[k] = merge(base[k], v)
    else result[k] = type(v) == "table" and merge(v, {}) or v end
  end
  -- Key arrays replace rather than append; false disables an action.
  if base.keys and override.keys then
    result.keys = merge(base.keys, {})
    for k, v in pairs(override.keys) do result.keys[k] = type(v) == "table" and merge(v, {}) or v end
  end
  return result
end

local function state(ctx)
  local session = assert(ctx.session, "jobs app requires a session")
  if not states[session] then states[session] = { cursor = 1, offset = 0, follow = true } end
  return states[session]
end

-- Trim only at UTF-8 boundaries. Inspection itself is already bounded by Rust;
-- this also bounds metadata, custom callbacks, and custom host implementations.
local function bounded(text, bytes, tail)
  text = tostring(text or "")
  if #text <= bytes then return text end
  if tail then
    local first = #text - bytes + 1
    while first <= #text and text:byte(first) >= 128 and text:byte(first) < 192 do first = first + 1 end
    return text:sub(first)
  end
  local last = bytes + 1
  while last > 1 and text:byte(last) >= 128 and text:byte(last) < 192 do last = last - 1 end
  return text:sub(1, last - 1)
end

local function plain(text)
  -- Do not feed escape sequences into the word wrapper (or terminal). Strip
  -- OSC/CSI before controls; never interpret process output as UI markup.
  return tostring(text or ""):gsub("\27%][^\7\27]*\7", "")
    :gsub("\27%][^\27]*\27\\", ""):gsub("\27%[[0-?]*[ -/]*[@-~]", "")
    :gsub("\27[ -/]*[@-~]", ""):gsub("\r\n", "\n"):gsub("\t", "    ")
    :gsub("[%z\1-\9\11-\31\127]", "")
end

local function wrapped(text, cols)
  local lines = {}
  for line in (plain(text) .. "\n"):gmatch("(.-)\n") do
    local parts = rness.ui.wrap(line, cols)
    if #parts == 0 then parts = { "" } end
    for _, part in ipairs(parts) do lines[#lines + 1] = part end
  end
  return lines
end

local function dimensions(ctx)
  return math.max(1, math.min(512, ctx.rows or options.layout.height)),
    math.max(1, math.min(4096, ctx.cols or 80))
end

local function model(ctx)
  local s = state(ctx)
  local rows, cols = dimensions(ctx)
  local m = { session = ctx.session, mode = s.selected and "detail" or "list",
    selected = s.selected, follow = s.follow, offset = s.offset,
    rows = rows, cols = cols, text = options.text }
  if s.selected then
    local ok, job = pcall(rness.jobs.inspect, ctx.session, s.selected)
    if ok then
      m.job = job
      if s.follow or s.snapshot == nil then s.snapshot = bounded(job.output, 8192, true) end
    else m.error = options.text.unavailable end
    m.output = s.snapshot or ""
    m.body = wrapped(m.output ~= "" and m.output or options.text.no_output, cols)
    m.page = math.max(1, rows - 4)
    local maximum = math.max(0, #m.body - m.page)
    s.offset = s.follow and maximum or math.min(s.offset, maximum)
    m.offset = s.offset
  else
    m.jobs = rness.jobs.list(ctx.session)
    -- Keep finished jobs visible and preserve selection as the registry changes.
    for i, job in ipairs(m.jobs) do if job.job_id == s.cursor_id then s.cursor = i; break end end
    s.cursor = math.max(1, math.min(s.cursor, #m.jobs))
    s.cursor_id = m.jobs[s.cursor] and m.jobs[s.cursor].job_id
    m.cursor = s.cursor
    m.page = math.max(1, rows - 2)
  end
  return m
end

local function default_view(m)
  local t, lines = options.text, {}
  if m.mode == "list" then
    if m.rows >= 2 then lines[1] = t.list .. " (" .. #m.jobs .. ")" end
    if #m.jobs == 0 then lines[#lines + 1] = t.empty end
    local first = math.max(1, m.cursor - m.page + 1)
    for i = first, math.min(#m.jobs, first + m.page - 1) do
      lines[#lines + 1] = (i == m.cursor and t.marker or "  ") .. plain(bounded(summary(m.jobs[i]), 1024)):gsub("\n", " ")
    end
    if m.rows >= 3 then lines[#lines + 1] = t.list_help end
  else
    -- Prefer actual output over chrome when the terminal has very few rows.
    if m.rows >= 2 then
      lines[#lines + 1] = m.job and plain(bounded(summary(m.job), 1024)):gsub("\n", " ") or t.unavailable
    end
    if m.rows >= 3 then
      lines[#lines + 1] = t.output .. " | " .. (m.follow and t.following or t.paused)
        .. " | " .. (m.offset + 1) .. "-" .. math.min(#m.body, m.offset + m.page) .. "/" .. #m.body
    end
    if m.rows >= 4 then lines[#lines + 1] = t.detail_help end
    for i = m.offset + 1, math.min(#m.body, m.offset + m.page) do lines[#lines + 1] = m.body[i] end
  end
  return lines
end

local function safe_lines(lines)
  assert(type(lines) == "table", "jobs view/render must return an array of strings")
  local out, remaining = {}, 65536
  for i, line in ipairs(lines) do
    if i > 512 or remaining <= 0 then break end
    assert(type(line) == "string", "jobs app lines must be strings")
    line = bounded(plain(bounded(line, remaining)), math.min(8192, remaining)):gsub("\n", " ")
    out[#out + 1], remaining = line, remaining - #line
  end
  -- Desired height must not collapse to the currently clipped viewport.
  while #out < options.layout.height do out[#out + 1] = "" end
  return out
end

local function view(ctx)
  if options.view then return safe_lines(options.view(ctx)) end
  local m = model(ctx)
  local lines = default_view(m)
  if options.render then lines = options.render(ctx, m, lines) or lines end
  return safe_lines(lines)
end

local function matches(action, key)
  local keys = options.keys[action]
  if type(keys) == "string" then return keys == key end
  for _, candidate in ipairs(keys or {}) do if candidate == key then return true end end
  return false
end

local function on_key(key, ctx)
  if key == " " then key = "space" end
  if options.on_key then
    local result = options.on_key(key, ctx)
    if result ~= nil then return result end
  end
  local s = state(ctx)
  if matches("back", key) then
    if not s.selected then
      if key == "esc" then return false end
      return "close"
    end
    s.selected, s.snapshot, s.offset, s.follow = nil, nil, 0, true
    return true
  end
  local action
  for _, name in ipairs({ "up", "down", "page_up", "page_down", "home", "follow", "open", "pause" }) do
    if matches(name, key) then action = name; break end
  end
  if not action then return false end
  local was_following = s.follow
  -- Freeze what was last displayed before inspecting again. New output may
  -- have arrived between the render and this scroll/pause keypress.
  if s.selected and action ~= "follow" and action ~= "open" then s.follow = false end
  local m = model(ctx)
  if s.selected then
    if action == "open" then return true end
    if action == "follow" or (action == "pause" and not was_following) then s.follow = true
    elseif action == "pause" then s.follow = false
    else
      s.follow = false
      local delta = ({ up = -1, down = 1, page_up = -m.page, page_down = m.page })[action] or 0
      s.offset = action == "home" and 0 or math.max(0, math.min(#m.body - m.page, s.offset + delta))
    end
  else
    if action == "open" and m.jobs[s.cursor] then
      s.selected, s.snapshot, s.offset, s.follow = m.jobs[s.cursor].job_id, nil, 0, true
    else
      local delta = ({ up = -1, down = 1, page_up = -m.page, page_down = m.page })[action] or 0
      s.cursor = action == "home" and 1 or action == "follow" and #m.jobs
        or math.max(1, math.min(#m.jobs, s.cursor + delta))
      s.cursor_id = m.jobs[s.cursor] and m.jobs[s.cursor].job_id
    end
  end
  return true
end

local function setup(config)
  assert(config == nil or type(config) == "table", "jobs.setup expects a table")
  local next_options = merge(defaults, config or {})
  assert(type(next_options.title) == "string", "jobs title must be a string")
  assert(type(next_options.refresh_ms) == "number" and next_options.refresh_ms >= 50
    and next_options.refresh_ms <= 60000 and next_options.refresh_ms % 1 == 0, "jobs refresh_ms must be 50..60000")
  assert(type(next_options.layout) == "table", "jobs layout must be a table")
  assert(type(next_options.layout.height) == "number" and next_options.layout.height >= 5
    and next_options.layout.height <= 512 and next_options.layout.height % 1 == 0, "jobs layout.height must be 5..512")
  for _, name in ipairs({ "view", "render", "on_key" }) do
    assert(next_options[name] == nil or type(next_options[name]) == "function", "jobs " .. name .. " must be a function")
  end
  for name, value in pairs(next_options.text) do assert(type(value) == "string", "jobs text." .. name .. " must be a string") end
  for name, keys in pairs(next_options.keys) do
    assert(defaults.keys[name], "unknown jobs key action: " .. name)
    assert(keys == false or type(keys) == "string" or type(keys) == "table", "jobs keys must be strings, arrays, or false")
    if type(keys) == "table" then for _, key in ipairs(keys) do assert(type(key) == "string", "jobs key must be a string") end end
  end
  local function help(actions, label)
    local keys = {}
    for _, action in ipairs(actions) do
      local value = next_options.keys[action]
      if type(value) == "string" then keys[#keys + 1] = value
      elseif type(value) == "table" then for _, key in ipairs(value) do keys[#keys + 1] = key end end
    end
    return #keys > 0 and (table.concat(keys, "/") .. ": " .. label) or nil
  end
  local function hints(parts)
    local result = {}
    for i = 1, parts.n do if parts[i] then result[#result + 1] = parts[i] end end
    return table.concat(result, " | ")
  end
  local custom_text = config and config.text or {}
  if custom_text.list_help == nil then
    next_options.text.list_help = hints(table.pack(help({"up", "down"}, "select"),
      help({"open"}, "output"), help({"back"}, "close")))
  end
  if custom_text.detail_help == nil then
    next_options.text.detail_help = hints(table.pack(help({"up", "down", "page_up", "page_down", "home"}, "scroll"),
      help({"pause"}, "pause/follow"), help({"follow"}, "follow"), help({"back"}, "list")))
  end
  rness.ui.app {
    name = "jobs", slot = "overlay", title = next_options.title,
    refresh_ms = next_options.refresh_ms, config = next_options.layout,
    capture_escape = true, view = view, on_key = on_key,
  }
  options = next_options
end
setup(rness.jobs.config)

local function open(ctx, id)
  local s = state(ctx)
  s.selected, s.snapshot, s.offset, s.follow = id, nil, 0, true
  return { data = { action = "app:open", app = "jobs", session = ctx.session } }
end

rness.commands.register {
  name = "jobs",
  description = "List background jobs, inspect recent output, or request a stop",
  usage = "[list | <job_id> | stop <job_id>]",
  arguments = { "list", "stop" },
  allow_busy = true,
  complete = function(ctx)
    local jobs = active_jobs(ctx.session)
    local raw = ctx.raw_input or ""
    local input = raw:match("^%s*(.-)%s*$")
    local exact, description = input == "", "Open jobs monitor"
    for _, job in ipairs(jobs) do
      if input == job.job_id then exact, description = true, hint(job); break end
    end
    -- Preserve exact known finished IDs without populating the active menu.
    if not exact and input ~= "list" and input ~= "stop" and not input:find("%s") then
      local ok, job = pcall(rness.jobs.inspect, ctx.session, input)
      if ok then exact, description = true, hint(job) end
    end
    local choices = {}
    -- The host prepends "jobs "; retain the exact open action before verbs,
    -- including trailing spaces used by completion's prefix filter.
    if exact then choices[#choices + 1] = choice(raw:gsub("^%s", "", 1), description) end
    choices[#choices + 1] = choice("list", "List active background jobs in this session")
    choices[#choices + 1] = choice("stop", "Choose a running job to stop")
    for _, job in ipairs(jobs) do
      local detail = hint(job)
      choices[#choices + 1] = choice(job.job_id, detail)
      choices[#choices + 1] = choice("stop " .. job.job_id, detail)
    end
    return choices
  end,
  run = function(ctx)
    local input = ctx.raw_input:match("^%s*(.-)%s*$")
    if input == "" then return open(ctx) end
    if input == "list" then
      local jobs = active_jobs(ctx.session)
      if #jobs == 0 then return { message = "No active background jobs in this session." } end
      local lines = { "Active background jobs in this session:" }
      for _, job in ipairs(jobs) do lines[#lines + 1] = summary(job) end
      lines[#lines + 1] = "Inspect: /jobs <job_id>   Stop: /jobs stop <job_id>"
      return { message = table.concat(lines, "\n"), data = jobs }
    end
    local id = input:match("^stop%s+(%S+)$")
    if id then
      local requested = rness.jobs.stop(ctx.session, id)
      return { message = requested and ("Cancellation requested for job " .. id
        .. ". It remains active until its process settles.")
        or ("Job " .. id .. " is already stopped or stopping.") }
    end
    assert(not input:find("%s") and input ~= "stop", usage)
    -- Validate visibility before touching selection; finished jobs remain valid.
    rness.jobs.inspect(ctx.session, input)
    return open(ctx, input)
  end,
}
