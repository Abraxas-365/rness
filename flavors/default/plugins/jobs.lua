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

-- The monitor. State is private to this plugin generation and scoped to the
-- command's session. Lines are strings or styled rows (see rness.ui.app).
local states = {}
local defaults = {
  title = "Background jobs", refresh_ms = 250,
  layout = {
    height = 24, title_style = "heading",
    border = { kind = "rounded", style = "overlay_border" },
  },
  keys = {
    up = { "up", "k" }, down = { "down", "j" },
    page_up = { "pageup" }, page_down = { "pagedown" },
    home = { "home" }, follow = { "end" }, open = { "enter" },
    back = { "esc" }, pause = { "space" }, stop = { "x" }, filter = { "f" },
  },
  text = {
    list = "", empty = "No background jobs in this session.",
    empty_filtered = "No running jobs. Press f to show finished jobs too.",
    list_help = nil, detail_help = nil,
    following = "FOLLOW", paused = "PAUSED", output = "",
    no_output = "(no output yet)", unavailable = "Job no longer available", marker = "▸ ",
    filter_all = "all", filter_running = "running only",
    confirm_stop = "Stop %s? Press x again to confirm, any other key cancels.",
    stop_requested = "Stop requested: %s. It stays listed until its process exits.",
    not_running = "That job has already finished.",
    already_stopping = "That job is already stopping.",
  },
  -- Theme group names or style tables ({ fg=, bg=, bold= ... }).
  styles = {
    running = "heading", stopping = "removed", ok = "added", failed = "error",
    stopped = "dim", kind = "tool_name", time = "dim", hint = "dim",
    header = "heading", notice = "heading", confirm = "error",
    selected = { bg = "#3c3836" }, marker = "heading",
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
  if not states[session] then
    states[session] = { cursor = 1, offset = 0, follow = true, filter = "all" }
  end
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

local function one_line(text)
  return (plain(bounded(text, 1024)):gsub("%s+", " "):match("^%s*(.-)%s*$"))
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

-- Status word, icon, and style for one job.
local function status_of(job)
  local st = options.styles
  if job.running then
    if job.cancellation_requested or job.status == "cancelling" then return "stopping", "◌", st.stopping end
    return "running", "●", st.running
  end
  if job.status == "exited" then
    if job.exit_code == 0 then return "exit 0", "✓", st.ok end
    if job.exit_code == nil then return "signal", "✗", st.failed end
    return "exit " .. job.exit_code, "✗", st.failed
  end
  if job.status == "killed" then return "killed", "■", st.stopped end
  return tostring(job.status), "!", st.stopped
end

-- Elapsed time while running; total run time once settled.
local function duration(job, now)
  local start = math.tointeger(job.started_at_ms)
  if not start then return "" end
  local finish = job.running and now or math.tointeger(job.settled_at_ms) or now
  local s = math.max(0, (finish - start) // 1000)
  if s < 60 then return s .. "s" end
  if s < 3600 then return string.format("%dm%02ds", s // 60, s % 60) end
  if s < 86400 then return string.format("%dh%02dm", s // 3600, s % 3600 // 60) end
  return string.format("%dd%02dh", s // 86400, s % 86400 // 3600)
end

-- Running jobs first, then finished; newest first within each group. Jobs
-- without timestamps keep registry order after timestamped ones.
local function sorted(jobs)
  local entries = {}
  for i, job in ipairs(jobs) do
    local key = job.running and job.started_at_ms or job.settled_at_ms or job.started_at_ms
    entries[i] = { job = job, index = i, key = key }
  end
  table.sort(entries, function(a, b)
    if a.job.running ~= b.job.running then return a.job.running end
    if a.key and b.key and a.key ~= b.key then return a.key > b.key end
    if (a.key ~= nil) ~= (b.key ~= nil) then return a.key ~= nil end
    return a.index < b.index
  end)
  local out = {}
  for i, entry in ipairs(entries) do out[i] = entry.job end
  return out
end

local function model(ctx)
  local s = state(ctx)
  local rows, cols = dimensions(ctx)
  local m = { session = ctx.session, mode = s.selected and "detail" or "list",
    selected = s.selected, follow = s.follow, offset = s.offset, filter = s.filter,
    confirm = s.confirm, confirm_label = s.confirm_label, notice = s.notice, now_ms = os.time() * 1000,
    rows = rows, cols = cols, text = options.text }
  -- Chrome: a header from 2 rows, a footer from 3; content gets the rest.
  m.header, m.footer = rows >= 2, rows >= 3
  local chrome = (m.header and 1 or 0) + (m.footer and 1 or 0)
  m.page = math.max(1, rows - chrome)
  if s.selected then
    local ok, job = pcall(rness.jobs.inspect, ctx.session, s.selected)
    if ok then
      m.job = job
      if s.follow or s.snapshot == nil then s.snapshot = bounded(job.output, 8192, true) end
    else m.error = options.text.unavailable end
    m.output = s.snapshot or ""
    m.body = wrapped(m.output ~= "" and m.output or options.text.no_output, cols)
    local maximum = math.max(0, #m.body - m.page)
    s.offset = s.follow and maximum or math.min(s.offset, maximum)
    m.offset = s.offset
  else
    local all = rness.jobs.list(ctx.session)
    m.total, m.running = #all, 0
    local visible = {}
    for _, job in ipairs(all) do
      if job.running then m.running = m.running + 1 end
      if s.filter ~= "running" or job.running then visible[#visible + 1] = job end
    end
    m.finished = m.total - m.running
    m.jobs = sorted(visible)
    -- Keep finished jobs visible and preserve selection as the registry changes.
    for i, job in ipairs(m.jobs) do if job.job_id == s.cursor_id then s.cursor = i; break end end
    s.cursor = math.max(1, math.min(s.cursor, #m.jobs))
    s.cursor_id = m.jobs[s.cursor] and m.jobs[s.cursor].job_id
    m.cursor = s.cursor
  end
  return m
end

local function job_row(m, job, selected, kind_width)
  local t, st = options.text, options.styles
  local word, icon, style = status_of(job)
  local kind = tostring(job.kind)
  if utf8.len(kind) and utf8.len(kind) > kind_width then
    kind = kind:sub(1, (utf8.offset(kind, kind_width) or (#kind + 1)) - 1)
  end
  local blank = string.rep(" ", utf8.len(t.marker) or #t.marker)
  return {
    { text = selected and t.marker or blank, style = st.marker },
    { text = icon .. " ", style = style },
    { text = kind .. string.rep(" ", kind_width - (utf8.len(kind) or #kind)) .. " ", style = st.kind },
    { text = string.format("%7s", duration(job, m.now_ms)) .. "  ", style = st.time },
    { text = one_line(job.label) },
    right = { { text = word, style = style } },
    style = selected and st.selected or nil,
  }
end

local function footer(m, help)
  local st = options.styles
  if m.confirm then
    local label = m.confirm_label or m.confirm
    return { text = string.format(options.text.confirm_stop, label), style = st.confirm }
  end
  if m.notice then return { text = m.notice, style = st.notice } end
  return { text = help, style = st.hint }
end

local function default_view(m)
  local t, st, lines = options.text, options.styles, {}
  if m.mode == "list" then
    if m.header then
      local head = {}
      if t.list ~= "" then head[#head + 1] = { text = t.list .. "  ", style = st.header } end
      head[#head + 1] = { text = "● " .. m.running .. " running", style = m.running > 0 and st.running or st.hint }
      head[#head + 1] = { text = "   ✓ " .. m.finished .. " finished", style = st.hint }
      head.right = { { text = "filter: " .. (m.filter == "running" and t.filter_running or t.filter_all), style = st.hint } }
      lines[#lines + 1] = head
    end
    if #m.jobs == 0 then
      lines[#lines + 1] = { text = (m.filter == "running" and m.total > 0) and t.empty_filtered or t.empty, style = st.hint }
    end
    local kind_width = 4
    for _, job in ipairs(m.jobs) do kind_width = math.max(kind_width, utf8.len(tostring(job.kind)) or 4) end
    kind_width = math.min(kind_width, 10)
    local first = math.max(1, m.cursor - m.page + 1)
    for i = first, math.min(#m.jobs, first + m.page - 1) do
      lines[#lines + 1] = job_row(m, m.jobs[i], i == m.cursor, kind_width)
    end
    if m.footer then
      while #lines < m.rows - 1 do lines[#lines + 1] = "" end
      lines[#lines + 1] = footer(m, t.list_help)
    end
  else
    -- Prefer actual output over chrome when the terminal has very few rows.
    if m.header then
      if m.job then
        local word, icon, style = status_of(m.job)
        local d = duration(m.job, m.now_ms)
        local position = (m.offset + 1) .. "-" .. math.min(#m.body, m.offset + m.page) .. "/" .. #m.body
        lines[#lines + 1] = {
          { text = icon .. " " .. word, style = style },
          { text = "  " .. tostring(m.job.kind), style = st.kind },
          { text = d ~= "" and ("  " .. d) or "", style = st.time },
          { text = "  " .. one_line(m.job.label) },
          right = {
            { text = tostring(m.job.job_id) .. "  ", style = st.hint },
            { text = (t.output ~= "" and (t.output .. "  ") or ""), style = st.hint },
            { text = m.follow and t.following or t.paused, style = m.follow and st.running or st.stopped },
            { text = "  " .. position, style = st.hint },
          },
        }
      else
        lines[#lines + 1] = { text = t.unavailable, style = st.failed }
      end
    end
    for i = m.offset + 1, math.min(#m.body, m.offset + m.page) do lines[#lines + 1] = m.body[i] end
    if m.footer then
      while #lines < m.rows - 1 do lines[#lines + 1] = "" end
      lines[#lines + 1] = footer(m, t.detail_help)
    end
  end
  return lines
end

local function safe_style(style)
  assert(style == nil or type(style) == "string" or type(style) == "table",
    "jobs line styles must be theme names or style tables")
  return style
end

-- Bound and sanitize any view: strings, spans {text=, style=}, or rows
-- (arrays of strings/spans with optional right= and style=).
local function safe_lines(lines)
  assert(type(lines) == "table", "jobs view/render must return an array of lines")
  local out, remaining = {}, 65536
  local function clean(text)
    local limit = math.max(0, remaining)
    text = (bounded(plain(bounded(text, limit)), math.min(8192, limit)):gsub("\n", " "))
    remaining = remaining - #text
    return text
  end
  local function span(value)
    if type(value) ~= "table" then return { text = clean(tostring(value)) } end
    return { text = clean(tostring(value.text or "")), style = safe_style(value.style) }
  end
  for i, line in ipairs(lines) do
    if i > 512 or remaining <= 0 then break end
    if type(line) == "string" then out[#out + 1] = clean(line)
    else
      assert(type(line) == "table", "jobs app lines must be strings or tables")
      if line.text ~= nil then out[#out + 1] = span(line)
      else
        local row = { style = safe_style(line.style) }
        for _, value in ipairs(line) do row[#row + 1] = span(value) end
        if line.right ~= nil then
          assert(type(line.right) == "table", "jobs row right= must be an array")
          row.right = {}
          for _, value in ipairs(line.right) do row.right[#row.right + 1] = span(value) end
        end
        out[#out + 1] = row
      end
    end
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

-- x: first press arms a confirmation for the selected job, second stops it.
local function stop_key(ctx, s, armed)
  local id = s.selected
  if not id then
    model(ctx)
    id = s.cursor_id
  end
  if not id then return true end
  local ok, job = pcall(rness.jobs.inspect, ctx.session, id)
  if not ok then s.notice = options.text.unavailable; return true end
  if not job.running then s.notice = options.text.not_running; return true end
  if job.cancellation_requested then s.notice = options.text.already_stopping; return true end
  local label = one_line(job.label)
  if label == "" then label = id end
  local cut = utf8.offset(label, 41)
  if cut and cut <= #label then label = label:sub(1, cut - 1) .. "…" end
  if armed == id then
    rness.jobs.stop(ctx.session, id)
    s.notice = string.format(options.text.stop_requested, label)
  else
    s.confirm, s.confirm_label = id, label
  end
  return true
end

local function on_key(key, ctx)
  if key == " " then key = "space" end
  if options.on_key then
    local result = options.on_key(key, ctx)
    if result ~= nil then return result end
  end
  local s = state(ctx)
  -- Any key dismisses a notice; any key but stop cancels a pending stop.
  local armed = s.confirm
  s.confirm, s.confirm_label, s.notice = nil, nil, nil
  if matches("stop", key) then return stop_key(ctx, s, armed) end
  if armed then return true end
  if matches("back", key) then
    if not s.selected then
      if key == "esc" then return false end
      return "close"
    end
    s.selected, s.snapshot, s.offset, s.follow = nil, nil, 0, true
    return true
  end
  if matches("filter", key) and not s.selected then
    s.filter = s.filter == "running" and "all" or "running"
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

local key_names = {
  up = "↑", down = "↓", left = "←", right = "→", enter = "⏎", pageup = "PgUp",
  pagedown = "PgDn", home = "Home", ["end"] = "End", space = "space", esc = "esc",
}

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
  for name, value in pairs(next_options.styles) do
    assert(type(value) == "string" or type(value) == "table", "jobs styles." .. name .. " must be a theme name or style table")
  end
  for name, keys in pairs(next_options.keys) do
    assert(defaults.keys[name], "unknown jobs key action: " .. name)
    assert(keys == false or type(keys) == "string" or type(keys) == "table", "jobs keys must be strings, arrays, or false")
    if type(keys) == "table" then for _, key in ipairs(keys) do assert(type(key) == "string", "jobs key must be a string") end end
  end
  -- Help shows the first key of each action: "↑/↓ select".
  local function help(actions, label)
    local keys = {}
    for _, action in ipairs(actions) do
      local value = next_options.keys[action]
      if type(value) == "table" then value = value[1] end
      if type(value) == "string" then keys[#keys + 1] = key_names[value] or value end
    end
    return #keys > 0 and (table.concat(keys, "/") .. " " .. label) or nil
  end
  local function hints(parts)
    local result = {}
    for i = 1, parts.n do if parts[i] then result[#result + 1] = parts[i] end end
    return table.concat(result, " · ")
  end
  local custom_text = config and config.text or {}
  if custom_text.list_help == nil then
    next_options.text.list_help = hints(table.pack(help({"up", "down"}, "select"),
      help({"open"}, "output"), help({"stop"}, "stop"), help({"filter"}, "filter"), help({"back"}, "close")))
  end
  if custom_text.detail_help == nil then
    next_options.text.detail_help = hints(table.pack(help({"up", "down", "page_up", "page_down"}, "scroll"),
      help({"pause"}, "pause"), help({"follow"}, "follow"), help({"stop"}, "stop"), help({"back"}, "back")))
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
  s.confirm, s.confirm_label, s.notice = nil, nil, nil
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
