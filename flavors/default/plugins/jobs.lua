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
    -- No fixed height: the panel fits its content between min/max_height.
    -- Set height = N for a fixed panel.
    min_height = 5, max_height = 24, title_style = "heading",
    border = { kind = "rounded", style = "overlay_border" },
  },
  list = {
    -- "active": running only. "recent": running plus recently finished.
    -- "all": every job this session still retains.
    filter = "recent",
    -- Within each group: "newest" or "oldest" first.
    order = "newest",
    -- Running / Recent sections. Without them, running_first still keeps
    -- live jobs above finished ones.
    group = true, running_first = true,
    -- Finished jobs shown by "recent": those settled in the last `secs`,
    -- but always the latest `min` and never more than `max`.
    recent = { secs = 1800, min = 3, max = 10 },
    -- Columns before "label" are left-aligned; those after it are
    -- right-aligned. Available: icon, id, kind, label, duration, age, status.
    columns = { "icon", "kind", "label", "duration", "age", "status" },
    -- Drop "cd <dir> &&", show the workspace as "." and $HOME as "~".
    shorten_labels = true,
    kind_names = { subagent = "agent" },
  },
  -- Optional function(job, label) -> string to format each list label.
  format_label = nil,
  keys = {
    up = { "up", "k" }, down = { "down", "j" },
    page_up = { "pageup" }, page_down = { "pagedown" },
    home = { "home" }, follow = { "end" }, open = { "enter" },
    back = { "esc" }, pause = { "space" }, stop = { "x" }, filter = { "f" },
    all = { "a" }, search = { "/" },
  },
  text = {
    list = "", empty = "No background jobs in this session.",
    empty_active = "No running jobs.",
    empty_recent = "Nothing running or recently finished.",
    no_matches = "No jobs match.",
    list_help = nil, detail_help = nil,
    following = "FOLLOW", paused = "PAUSED", output = "",
    no_output = "(no output yet)", unavailable = "Job no longer available", marker = "▸ ",
    running_count = "● %d running", none_running = "○ nothing running",
    finished_count = "%d finished",
    filter_label = "showing ", filter_active = "running", filter_recent = "recent", filter_all = "all",
    section_running = "Running", section_recent = "Recent", section_finished = "Finished",
    more_recent = "… %d older hidden", more_active = "… %d finished hidden",
    search_prompt = "/", search_cursor = "▏",
    just_now = "now", ago = "%s ago",
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
    section = "heading", search = "heading", label = nil,
  },
}
local options
local function merge(base, override)
  local result = {}
  for k, v in pairs(base) do result[k] = type(v) == "table" and merge(v, {}) or v end
  for k, v in pairs(override) do
    -- Maps merge; arrays (columns, key lists) replace.
    if type(v) == "table" and type(base[k]) == "table" and k ~= "keys" and base[k][1] == nil then
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

local function fresh_list(s)
  s.cursor, s.cursor_id, s.top = 1, nil, 1
  s.filter, s.query, s.searching = options.list.filter, "", false
end

local function state(ctx)
  local session = assert(ctx.session, "jobs app requires a session")
  if not states[session] then
    states[session] = { offset = 0, follow = true }
    fresh_list(states[session])
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

local function width(text) return utf8.len(text) or #text end
local function pad(text, n) return text .. string.rep(" ", math.max(0, n - width(text))) end
local function truncate(text, n)
  if width(text) <= n then return text end
  local cut = utf8.offset(text, math.max(1, n)) or (#text + 1)
  return text:sub(1, cut - 1) .. "…"
end

-- Content rows. The view always pads to `desired` rows, which the host uses
-- as the panel height; ctx.rows is the viewport of the last render (smaller
-- when the terminal clipped it, or for one frame after the list grew).
local function rows_for(ctx, desired)
  local cols = math.max(1, math.min(4096, ctx.cols or 80))
  desired = options.layout.height or desired
  return math.max(1, math.min(512, desired, ctx.rows or desired)), cols, desired
end

-- Job IDs are "j" + ULID; legacy snapshots without started_at_ms still carry
-- their start time in the ID.
local CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
local function ulid_ms(id)
  local body = tostring(id or ""):match("^j?([0-9A-HJKMNP-TV-Z]+)$")
  if not body or #body ~= 26 then return nil end
  local ms = 0
  for i = 1, 10 do ms = ms * 32 + (CROCKFORD:find(body:sub(i, i), 1, true) - 1) end
  return ms
end

local function started_ms(job) return math.tointeger(job.started_at_ms) or ulid_ms(job.job_id) end
local function ended_ms(job) return not job.running and math.tointeger(job.settled_at_ms) or nil end
local function recency(job)
  if job.running then return started_ms(job) end
  return ended_ms(job) or started_ms(job)
end

-- Status word, icon, and style for one job.
local function status_of(job)
  local st = options.styles
  if job.running then
    if job.cancellation_requested or job.status == "cancelling" then return "stopping", "◌", st.stopping end
    return "running", "●", st.running
  end
  if job.status == "exited" then
    if job.exit_code == 0 then return "done", "✓", st.ok end
    if job.exit_code == nil then return "signal", "✗", st.failed end
    return "exit " .. job.exit_code, "✗", st.failed
  end
  if job.status == "killed" then return "killed", "■", st.stopped end
  return tostring(job.status), "!", st.stopped
end

local function compact(s)
  if s < 60 then return s .. "s" end
  if s < 3600 then return string.format("%dm%02ds", s // 60, s % 60) end
  if s < 86400 then return string.format("%dh%02dm", s // 3600, s % 3600 // 60) end
  return string.format("%dd%02dh", s // 86400, s % 86400 // 3600)
end

-- Elapsed time while running; total run time once settled. Legacy records
-- lack started_at_ms, and their ID time can predate settlement by days, so
-- they show no duration rather than a misleading one.
local function duration(job, now)
  local start = math.tointeger(job.started_at_ms)
  if not start and job.running then start = ulid_ms(job.job_id) end
  local finish = job.running and now or ended_ms(job)
  if not start or not finish then return "" end
  return compact(math.max(0, (finish - start) // 1000))
end

-- How long ago a finished job ended ("12m ago"); blank while running.
local function age(job, now)
  if job.running then return "" end
  local at = ended_ms(job) or started_ms(job)
  if not at then return "" end
  local s = math.max(0, (now - at) // 1000)
  if s < 5 then return options.text.just_now end
  local rough = s < 60 and (s .. "s") or s < 3600 and ((s // 60) .. "m")
    or s < 86400 and ((s // 3600) .. "h") or ((s // 86400) .. "d")
  return string.format(options.text.ago, rough)
end

local function escape(text) return (text:gsub("[%^%$%(%)%%%.%[%]%*%+%-%?]", "%%%0")) end
-- Replace a directory prefix only at path boundaries on both sides, so
-- /mnt/backup/proj is not shortened when the directory is /proj.
local function replace_dir(text, dir, short)
  if not dir or #dir < 2 then return text end
  return (text:gsub("()" .. escape(dir) .. "(.?)", function(at, next_char)
    local before = at > 1 and text:sub(at - 1, at - 1) or ""
    if before ~= "" and not before:match("[%s\"'`=:(;|&]") then return nil end
    if next_char == "/" then return short == "." and "" or (short .. "/") end
    if next_char == "" or next_char:match("[%s\"'`;)|&]") then return short .. next_char end
    return nil
  end))
end

local function label_of(job)
  local label = one_line(job.label)
  if options.list.shorten_labels then
    label = label:gsub("^subagent %[[%w%-_]+%]:%s*", "")
    label = label:gsub("^cd%s+%S+%s*&&%s*", "")
    label = replace_dir(label, os.getenv("PWD"), ".")
    label = replace_dir(label, os.getenv("HOME"), "~")
  end
  if options.format_label then
    local custom = options.format_label(job, label)
    if custom ~= nil then label = one_line(custom) end
  end
  return label
end

local function kind_of(job)
  local kind = tostring(job.kind)
  return options.list.kind_names[kind] or kind
end

local function searchable(job)
  return (tostring(job.label) .. " " .. label_of(job) .. " " .. tostring(job.kind) .. " " .. kind_of(job)
    .. " " .. tostring(job.job_id)):lower()
end

-- Group, filter, search and order the registry into display entries:
-- { section = text, count = n } | { job = job } | { more = n }.
local function build_list(m, s, all)
  local list, now = options.list, m.now_ms
  local query = (s.query or ""):lower()
  local running, finished = {}, {}
  m.total, m.running = #all, 0
  for i, job in ipairs(all) do
    if job.running then m.running = m.running + 1 end
    if query == "" or searchable(job):find(query, 1, true) then
      local entry = { job = job, index = i, key = recency(job) }
      if job.running then running[#running + 1] = entry else finished[#finished + 1] = entry end
    end
  end
  m.finished = m.total - m.running
  local function ordered(newest)
    return function(a, b)
      if a.key and b.key and a.key ~= b.key then return (a.key > b.key) == newest end
      if (a.key ~= nil) ~= (b.key ~= nil) then return a.key ~= nil end
      if newest then return a.index > b.index end
      return a.index < b.index
    end
  end
  local cmp = ordered(list.order ~= "oldest")
  -- Searching looks through everything; filters only shape the idle view.
  local scope = query ~= "" and "all" or s.filter
  local shown = {}
  if scope == "all" then shown = finished
  elseif scope == "recent" then
    table.sort(finished, ordered(true))
    for i, entry in ipairs(finished) do
      if #shown >= list.recent.max then break end
      local seconds = entry.key and (now - entry.key) // 1000
      if i <= list.recent.min or (seconds and seconds <= list.recent.secs) then shown[#shown + 1] = entry end
    end
  end
  m.hidden = #finished - #shown
  table.sort(running, cmp)
  table.sort(shown, cmp)
  local entries, jobs = {}, {}
  local function add(group)
    for _, entry in ipairs(group) do
      jobs[#jobs + 1] = entry.job
      entries[#entries + 1] = { job = entry.job, position = #jobs }
    end
  end
  if list.group then
    if #running > 0 then
      entries[#entries + 1] = { section = options.text.section_running, count = #running }
      add(running)
    end
    if #shown > 0 then
      entries[#entries + 1] = { section = scope == "all" and options.text.section_finished
        or options.text.section_recent, count = #shown }
      add(shown)
    end
  else
    local merged = {}
    for _, entry in ipairs(running) do merged[#merged + 1] = entry end
    for _, entry in ipairs(shown) do merged[#merged + 1] = entry end
    if not list.running_first then table.sort(merged, cmp) end
    add(merged)
  end
  if m.hidden > 0 then entries[#entries + 1] = { more = m.hidden } end
  m.entries, m.jobs, m.scope = entries, jobs, scope
end

local function model(ctx)
  local s = state(ctx)
  local m = { session = ctx.session, mode = s.selected and "detail" or "list",
    selected = s.selected, follow = s.follow, offset = s.offset, filter = s.filter,
    query = s.query, searching = s.searching,
    confirm = s.confirm, confirm_label = s.confirm_label, notice = s.notice, now_ms = os.time() * 1000,
    text = options.text }
  local layout = options.layout
  if s.selected then
    m.rows, m.cols, m.height = rows_for(ctx, layout.max_height)
  else
    build_list(m, s, rness.jobs.list(ctx.session))
    -- Header + content + footer, within the configured bounds.
    local desired = 2 + math.max(1, #m.entries)
    desired = math.max(layout.min_height, math.min(layout.max_height, desired))
    m.rows, m.cols, m.height = rows_for(ctx, desired)
  end
  -- Chrome: a header from 2 rows, a footer from 3; content gets the rest.
  m.header, m.footer = m.rows >= 2, m.rows >= 3
  local chrome = (m.header and 1 or 0) + (m.footer and 1 or 0)
  m.page = math.max(1, m.rows - chrome)
  if not s.selected and m.page < 3 then
    -- Too small for section chrome: jobs only.
    local only = {}
    for _, entry in ipairs(m.entries) do if entry.job then only[#only + 1] = entry end end
    m.entries = only
  end
  if s.selected then
    local ok, job = pcall(rness.jobs.inspect, ctx.session, s.selected)
    if ok then
      m.job = job
      if s.follow or s.snapshot == nil then s.snapshot = bounded(job.output, 8192, true) end
    else m.error = options.text.unavailable end
    m.output = s.snapshot or ""
    m.body = wrapped(m.output ~= "" and m.output or options.text.no_output, m.cols)
    local maximum = math.max(0, #m.body - m.page)
    s.offset = s.follow and maximum or math.min(s.offset, maximum)
    m.offset = s.offset
  else
    -- Selection follows job identity as the registry and filters change.
    local found
    for i, job in ipairs(m.jobs) do if job.job_id == s.cursor_id then found = i; break end end
    s.cursor = math.max(1, math.min(found or s.cursor, #m.jobs))
    s.cursor_id = m.jobs[s.cursor] and m.jobs[s.cursor].job_id
    m.cursor = s.cursor
    -- Scroll the entry window so the selection (and its section header, and
    -- a trailing "more" row) stays visible.
    local at
    for i, entry in ipairs(m.entries) do if entry.position == m.cursor then at = i; break end end
    local top = s.top or 1
    if at then
      local first = (m.entries[at - 1] and m.entries[at - 1].section) and at - 1 or at
      local last = (m.entries[at + 1] and m.entries[at + 1].more) and at + 1 or at
      if last > top + m.page - 1 then top = last - m.page + 1 end
      if first < top then top = first end
    end
    s.top = math.max(1, math.min(top, math.max(1, #m.entries - m.page + 1)))
    m.top = s.top
  end
  return m
end

local function cell(m, job, column, widths)
  local st = options.styles
  local word, icon, style = status_of(job)
  if column == "icon" then return { text = icon .. " ", style = style } end
  if column == "id" then return { text = pad(tostring(job.job_id), widths.id) .. "  ", style = st.hint } end
  if column == "kind" then return { text = pad(truncate(kind_of(job), widths.kind), widths.kind) .. "  ", style = st.kind } end
  if column == "label" then return { text = label_of(job), style = st.label } end
  if column == "duration" then return { text = string.format("%7s", duration(job, m.now_ms)), style = job.running and style or st.time } end
  if column == "age" then return { text = string.format("%9s", age(job, m.now_ms)), style = st.time } end
  if column == "status" then return { text = "  " .. string.rep(" ", widths.status - width(word)) .. word, style = style } end
end

local function job_row(m, job, selected, widths)
  local t, st = options.text, options.styles
  local blank = string.rep(" ", width(t.marker))
  local row = { { text = selected and t.marker or blank, style = st.marker },
    right = {}, style = selected and st.selected or nil }
  local right = false
  for _, column in ipairs(options.list.columns) do
    -- Narrow panels keep the label readable: drop time columns first.
    local skip = m.cols < 60 and (column == "duration" or column == "age")
    if not skip then
      local value = cell(m, job, column, widths)
      if right then row.right[#row.right + 1] = value else row[#row + 1] = value end
    end
    if column == "label" then right = true end
  end
  if #row.right == 0 then row.right = nil end
  return row
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

local function list_header(m)
  local t, st = options.text, options.styles
  local head = {}
  if t.list ~= "" then head[#head + 1] = { text = t.list .. "  ", style = st.header } end
  if m.running > 0 then
    head[#head + 1] = { text = string.format(t.running_count, m.running), style = st.running }
  else
    head[#head + 1] = { text = t.none_running, style = st.hint }
  end
  head[#head + 1] = { text = "  ·  " .. string.format(t.finished_count, m.finished), style = st.hint }
  if m.searching or (m.query or "") ~= "" then
    head.right = { { text = t.search_prompt .. m.query .. (m.searching and t.search_cursor or ""), style = st.search } }
  else
    head.right = { { text = t.filter_label .. (t["filter_" .. m.filter] or m.filter), style = st.hint } }
  end
  return head
end

local function list_view(m)
  local t, st, lines = options.text, options.styles, {}
  if m.header then lines[#lines + 1] = list_header(m) end
  local widths = { kind = 4, status = 4, id = 4 }
  for _, job in ipairs(m.jobs) do
    widths.kind = math.max(widths.kind, width(kind_of(job)))
    widths.status = math.max(widths.status, width((status_of(job))))
    widths.id = math.max(widths.id, width(tostring(job.job_id)))
  end
  widths.kind = math.min(widths.kind, 10)
  local blank = string.rep(" ", width(t.marker))
  if #m.jobs == 0 then
    local empty = m.total == 0 and t.empty or (m.query or "") ~= "" and t.no_matches
      or m.scope == "active" and t.empty_active or t.empty_recent
    lines[#lines + 1] = { text = blank .. empty, style = st.hint }
  end
  for i = m.top, math.min(#m.entries, m.top + m.page - 1 - (#m.jobs == 0 and 1 or 0)) do
    local entry = m.entries[i]
    if entry.section then
      lines[#lines + 1] = { { text = entry.section, style = st.section },
        { text = " · " .. entry.count, style = st.hint } }
    elseif entry.more then
      local text = string.format(m.scope == "active" and t.more_active or t.more_recent, entry.more)
      if t.all_hint ~= "" then text = text .. "  ·  " .. t.all_hint end
      lines[#lines + 1] = { text = blank .. text, style = st.hint }
    else
      lines[#lines + 1] = job_row(m, entry.job, entry.position == m.cursor, widths)
    end
  end
  if m.footer then
    while #lines < m.rows - 1 do lines[#lines + 1] = "" end
    lines[#lines + 1] = footer(m, t.list_help)
  end
  return lines
end

local function detail_view(m)
  local t, st, lines = options.text, options.styles, {}
  -- Prefer actual output over chrome when the terminal has very few rows.
  if m.header then
    if m.job then
      local word, icon, style = status_of(m.job)
      local d = duration(m.job, m.now_ms)
      local position = (m.offset + 1) .. "-" .. math.min(#m.body, m.offset + m.page) .. "/" .. #m.body
      lines[#lines + 1] = {
        { text = icon .. " " .. word, style = style },
        { text = "  " .. kind_of(m.job), style = st.kind },
        { text = d ~= "" and ("  " .. d) or "", style = st.time },
        { text = "  " .. label_of(m.job) },
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
  return lines
end

local function default_view(m)
  if m.mode == "list" then return list_view(m) end
  return detail_view(m)
end

local function safe_style(style)
  assert(style == nil or type(style) == "string" or type(style) == "table",
    "jobs line styles must be theme names or style tables")
  return style
end

-- Bound and sanitize any view: strings, spans {text=, style=}, or rows
-- (arrays of strings/spans with optional right= and style=).
local function safe_lines(lines, height)
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
  -- The requested height must not collapse to a clipped viewport.
  while #out < math.min(512, height or 0) do out[#out + 1] = "" end
  return out
end

local function view(ctx)
  if options.view then return safe_lines(options.view(ctx), options.layout.height) end
  local m = model(ctx)
  local lines = default_view(m)
  if options.render then lines = options.render(ctx, m, lines) or lines end
  return safe_lines(lines, m.height)
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
  local label = label_of(job)
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

-- While searching, printable keys edit the query; arrows still navigate.
local function search_key(key, s)
  local printable = key == "space" or utf8.len(key) == 1
  if key == "esc" or (not printable and matches("back", key)) then s.searching, s.query = false, ""
  elseif key == "enter" or (not printable and matches("open", key)) then s.searching = false
  elseif key == "backspace" then
    local last = utf8.offset(s.query, -1)
    s.query = last and s.query:sub(1, last - 1) or ""
  elseif key == "ctrl+u" then s.query = ""
  elseif printable then
    s.query = bounded(s.query .. (key == "space" and " " or key), 256)
  else return nil end
  s.cursor, s.cursor_id, s.top = 1, nil, 1
  return true
end

local filter_cycle = { active = "recent", recent = "all", all = "active" }

local function on_key(key, ctx)
  if key == " " then key = "space" end
  if options.on_key then
    local result = options.on_key(key, ctx)
    if result ~= nil then return result end
  end
  local s = state(ctx)
  if s.searching and not s.selected then
    local handled = search_key(key, s)
    if handled ~= nil then return handled end
  end
  -- Any key dismisses a notice; any key but stop cancels a pending stop.
  local armed = s.confirm
  s.confirm, s.confirm_label, s.notice = nil, nil, nil
  if matches("stop", key) then return stop_key(ctx, s, armed) end
  if armed then return true end
  if matches("back", key) then
    if not s.selected then
      if s.query ~= "" then s.query, s.top = "", 1; return true end
      if key == "esc" then return false end
      return "close"
    end
    s.selected, s.snapshot, s.offset, s.follow = nil, nil, 0, true
    return true
  end
  if not s.selected then
    -- A search covers every job, so filtering first leaves the search.
    if matches("filter", key) then
      s.query, s.top = "", 1
      s.filter = filter_cycle[s.filter] or "recent"; return true
    end
    if matches("all", key) then
      s.query, s.top = "", 1
      local default = options.list.filter
      s.filter = s.filter ~= "all" and "all" or (default ~= "all" and default or "recent")
      return true
    end
    if matches("search", key) then s.searching = true; return true end
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
      s.searching = false
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

local column_names = { icon = true, id = true, kind = true, label = true, duration = true, age = true, status = true }

local function validate_list(list)
  assert(type(list) == "table", "jobs list must be a table")
  assert(({ active = true, recent = true, all = true })[list.filter], "jobs list.filter must be active, recent, or all")
  assert(list.order == "newest" or list.order == "oldest", "jobs list.order must be newest or oldest")
  for _, name in ipairs({ "group", "running_first", "shorten_labels" }) do
    assert(type(list[name]) == "boolean", "jobs list." .. name .. " must be a boolean")
  end
  local recent = list.recent
  assert(type(recent) == "table", "jobs list.recent must be a table")
  for _, name in ipairs({ "secs", "min", "max" }) do
    assert(math.tointeger(recent[name]) and recent[name] >= 0, "jobs list.recent." .. name .. " must be a non-negative integer")
  end
  assert(type(list.columns) == "table" and #list.columns > 0, "jobs list.columns must be a non-empty array")
  for _, column in ipairs(list.columns) do
    assert(column_names[column], "unknown jobs column: " .. tostring(column))
  end
  assert(type(list.kind_names) == "table", "jobs list.kind_names must be a table")
  for kind, name in pairs(list.kind_names) do
    assert(type(kind) == "string" and type(name) == "string", "jobs list.kind_names must map strings to strings")
  end
end

local function setup(config)
  assert(config == nil or type(config) == "table", "jobs.setup expects a table")
  local next_options = merge(defaults, config or {})
  assert(type(next_options.title) == "string", "jobs title must be a string")
  assert(type(next_options.refresh_ms) == "number" and next_options.refresh_ms >= 50
    and next_options.refresh_ms <= 60000 and next_options.refresh_ms % 1 == 0, "jobs refresh_ms must be 50..60000")
  local layout = next_options.layout
  assert(type(layout) == "table", "jobs layout must be a table")
  assert(layout.height == nil or (type(layout.height) == "number" and layout.height >= 5
    and layout.height <= 512 and layout.height % 1 == 0), "jobs layout.height must be 5..512")
  -- The host sizes an unfixed overlay to its view, at most 30 rows with borders.
  for _, name in ipairs({ "min_height", "max_height" }) do
    assert(math.tointeger(layout[name]) and layout[name] >= 3 and layout[name] <= 28,
      "jobs layout." .. name .. " must be 3..28")
  end
  assert(layout.min_height <= layout.max_height, "jobs layout.min_height must not exceed max_height")
  validate_list(next_options.list)
  for _, name in ipairs({ "view", "render", "on_key", "format_label" }) do
    assert(next_options[name] == nil or type(next_options[name]) == "function", "jobs " .. name .. " must be a function")
  end
  for name, value in pairs(next_options.text) do assert(type(value) == "string", "jobs text." .. name .. " must be a string") end
  -- Older configs named these differently; keep honouring them.
  local legacy = config and config.text or {}
  if legacy.filter_running and not legacy.filter_active then next_options.text.filter_active = legacy.filter_running end
  if legacy.empty_filtered and not legacy.empty_active then next_options.text.empty_active = legacy.empty_filtered end
  -- Texts fed to string.format must accept their argument, or every refresh would fail.
  for name, sample in pairs({ running_count = 1, finished_count = 1, more_recent = 1, more_active = 1,
    ago = "1m", confirm_stop = "job", stop_requested = "job" }) do
    assert(pcall(string.format, next_options.text[name], sample),
      "jobs text." .. name .. " is not a valid format string (use %% for a literal %)")
  end
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
      help({"open"}, "output"), help({"stop"}, "stop"), help({"filter"}, "filter"),
      help({"search"}, "search"), help({"back"}, "close")))
  end
  if custom_text.detail_help == nil then
    next_options.text.detail_help = hints(table.pack(help({"up", "down", "page_up", "page_down"}, "scroll"),
      help({"pause"}, "pause"), help({"follow"}, "follow"), help({"stop"}, "stop"), help({"back"}, "back")))
  end
  if custom_text.all_hint == nil then next_options.text.all_hint = help({"all"}, "show all") or "" end
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
  fresh_list(s)
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
