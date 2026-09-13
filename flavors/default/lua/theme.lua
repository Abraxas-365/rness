local p = {
  bg = "#282828", card = "#3c3836", fg = "#ebdbb2", muted = "#a89984",
  border = "#665c54", yellow = "#fabd2f", green = "#b8bb26",
  red = "#fb4934", blue = "#83a598", purple = "#d3869b",
}
-- Pass file extensions to the renderer's syntax registry; unknown types stay plain.
local function file_language(path)
  if type(path) ~= "string" then return nil end
  return path:match("%.([^./\\]+)$")
end

-- Compact summaries stay single-line and never split a UTF-8 character.
local function preview(text, limit, fallback)
  if type(text) ~= "string" then return fallback or "" end
  text = text:gsub("%s+", " "):match("^%s*(.-)%s*$")
  if text == "" then return fallback or "" end
  local beyond = utf8.offset(text, limit + 1)
  if beyond and beyond <= #text then
    return text:sub(1, utf8.offset(text, limit) - 1) .. "…"
  end
  return text
end

local function tool_status(call)
  if call.is_error then return "failed", "error" end
  if type(call.presentation) == "table" and call.presentation.status == "running" then
    return "running", { fg = p.yellow, bold = true }
  end
  return "done", { fg = p.green, bold = true }
end

local function web_card(call)
  local search = call.name == "web_search"
  local title = search and "Web search" or "Web fetch"
  local args = call.args or {}
  local output = call.output or ""
  local payload = output:match("^%s*({.*})%s*$") or output:match("\n%s*({.*})%s*$")
  local ok, data = pcall(rness.json.decode, payload or "")
  local body = {}
  local function line(text, style)
    if type(text) == "string" and text ~= "" then
      body[#body + 1] = { text = text, style = style or "tool_output" }
    end
  end
  line(search and args.query or args.url, "tool_name")
  if call.is_error or not ok or type(data) ~= "table" then
    line(output, call.is_error and "error" or "tool_output")
  else
    line("External web content · untrusted", "dim")
    if search then
      local sources = type(data.sources) == "table" and data.sources or {}
      title = title .. " · " .. #sources .. " sources"
      if #sources == 0 then line("No sources returned", "dim") end
      for i, source in ipairs(sources) do
        line(string.format("%d. %s", i, source.title or source.url or "Source"), "tool_name")
        line(source.url, "dim")
        line(source.snippet)
      end
    else
      if data.url ~= args.url then line(data.url, "dim") end
      line(data.contentType, "dim")
    end
    line(data.content)
    if data.truncated then line("Content truncated by web tool limits", "dim") end
  end
  local status = tool_status(call)
  return { header = { text = title .. (status == "failed" and " · failed" or status == "running" and " · running" or ""), style = call.is_error and "error" or "tool_name" }, body = body }
end

rness.ui.colorscheme.register("gruvbox", {
  assistant_text = { fg = p.fg, bg = p.bg },
  user_prefix = { fg = p.yellow, bold = true },
  thinking = { fg = p.muted, italic = true },
  tool_name = { fg = p.blue, bold = true },
  tool_output = { fg = p.fg, bg = p.card },
  error = { fg = p.red, bold = true },
  added = { fg = p.green }, removed = { fg = p.red },
  heading = { fg = p.yellow, bold = true },
  code = { fg = p.green }, code_block = { fg = p.fg, bg = p.card },
  dim = { fg = p.muted },
  editor_prompt = { fg = p.yellow, bg = p.card, bold = true },
  statusline = { fg = p.muted, bg = p.card },
  statusline_accent = { fg = p.yellow, bg = p.card, bold = true },
  overlay = { fg = p.fg, bg = p.bg },
  overlay_border = { fg = p.border, bg = p.bg },
})
rness.ui.colorscheme.set("gruvbox")
rness.ui.messagebox = {
  -- Read-only agent drawer; omitted fields retain built-in defaults.
  agents = {
    keys = {
      back = "esc", previous_agent = "shift+tab", next_agent = "tab",
      list_up = { "up", "k" }, list_down = { "down", "j" }, open_detail = "enter",
      metadata_up = "alt+pageup", metadata_down = "alt+pagedown",
      page_up = "pageup", page_down = "pagedown", follow = "end",
      scroll_up = "up", scroll_down = "down", toggle_tool = { "enter", "ctrl+o" },
    },
    layout = { metadata_rows = 3, wheel_lines = 3, page_lines = 10, metadata_scroll_lines = 3 },
    text = { title = "Agents", incoming_label = "Incoming message" },
    styles = { frame = "overlay", border = "overlay_border", heading = "heading", hint = "dim" },
  },
  selection = {
    style = { bg = "#504945" },
    marker = { text = "▎", style = { fg = p.blue, bold = true } },
  },
  keys = {
    select_message = "alt+m",
  },
  user = {
    style = { fg = p.fg, bg = p.card }, padding = { left = 1, right = 1 },
    marker = false, label = { text = "You", style = "user_prefix" },
  },
  assistant = { style = "assistant_text", marker = false },
  thinking = { display = "preview", preview_lines = 3, style = "thinking" },
  compaction = {
    style = { fg = p.fg, bg = p.card },
    border = { kind = "rounded", style = { fg = p.border } },
    padding = { left = 1, right = 1 }, display = "preview", preview_lines = 6,
    header = { style = "tool_name" },
  },
  tool = {
    style = { fg = p.fg, bg = p.card },
    border = { kind = "rounded", style = { fg = p.border } },
    padding = { left = 1, right = 1 }, display = "preview", preview_lines = 8,
    header = { show_name = true, show_status = true, show_duration = true },
    states = { error = { display = "expanded", style = "error" } },
  },
  error = { style = "error", display = "expanded" },
  tools = {
    web_fetch = { display = "preview", preview_lines = 12, render = web_card },
    web_search = { display = "preview", preview_lines = 12, render = web_card },
    subagent = { display = "preview", preview_lines = 6, render = function(call)
      local meta = call.presentation
      if type(meta) ~= "table" or meta.kind ~= "subagent_activity" then return nil end
      local args = type(call.args) == "table" and call.args or {}
      local failed = call.is_error or meta.status == "error" or meta.status == "failed"
      local state = call.is_error and "failed" or preview(meta.status, 24, "unknown")
      if state == "completed" then
        state = (meta.mode or args.background_mode or args.mode) == "continuable" and "idle" or "finished"
      end
      local identity = preview(args.agent, 48, "subagent")
      local session = type(meta.session) == "string" and meta.session or ""
      local length = utf8.len(session)
      if length and length > 8 then session = session:sub(utf8.offset(session, -8)) end
      session = preview(session, 9)
      if session ~= "" then identity = identity .. " · " .. session end
      local elapsed = tonumber(meta.elapsed_ms)
      local duration = elapsed and elapsed >= 0 and elapsed < math.huge
        and string.format(" · %ds", math.floor(elapsed / 1000)) or ""
      local body = { { text = "Task: " .. preview(args.prompt, 240, "Not available"), style = "dim" } }
      local tools = type(meta.tools) == "table" and meta.tools or nil
      local counts = { running = 0, done = 0, failed = 0, cancelled = 0 }
      local total, current = 0, nil
      for _, tool in ipairs(tools or {}) do
        if type(tool) == "table" then
          total = total + 1
          local status = type(tool.status) == "string" and tool.status or "running"
          if counts[status] then counts[status] = counts[status] + 1 end
          if status == "running" then current = preview(tool.name, 80, "tool") end
        end
      end
      -- Older metadata has only activity/live text. Never flatten lines,
      -- streams, or nested tool arguments/results into the parent card.
      body[#body + 1] = { text = "Activity: " .. (current or preview(meta.activity, 160, state)), style = "tool_name" }
      if tools then
        body[#body + 1] = { text = string.format("Recent tools: %d · %d running · %d done · %d failed · %d cancelled",
          total, counts.running, counts.done, counts.failed, counts.cancelled), style = counts.failed > 0 and "error" or "dim" }
      end
      local live = preview(meta.live, 160)
      if live ~= "" then body[#body + 1] = { text = live, style = "assistant_text" } end
      if call.is_error then
        body[#body + 1] = { text = preview(call.output, 200, "Subagent failed"), style = "error" }
      end
      body[#body + 1] = { text = "Inspect: select card, i", style = "dim" }
      return {
        header = { text = identity .. " · " .. state .. duration, style = failed and "error" or "tool_name" },
        body = body,
      }
    end },
    send_message = { display = "preview", preview_lines = 4, render = function(call)
      local args = type(call.args) == "table" and call.args or {}
      local meta = type(call.presentation) == "table" and call.presentation or {}
      local failed = call.is_error or meta.accepted == false
      local state = failed and "failed" or meta.status == "running" and "running" or "Accepted"
      local recipient = preview(args.agent_id, 80, preview(meta.agent_id, 80, "Unknown recipient"))
      local body = { { text = type(args.message) == "string" and args.message or "Message unavailable", style = "tool_output" } }
      if failed then
        body[#body + 1] = { text = type(call.output) == "string" and call.output ~= "" and call.output or "Message not accepted", style = "error" }
      end
      return {
        header = { text = "Send message → " .. recipient .. " · " .. state, style = failed and "error" or "tool_name" },
        body = body,
      }
    end },
    Bash = { render = function(call)
      local args = type(call.args) == "table" and call.args or {}
      local command = type(args.command) == "string" and args.command or ""
      local duration = call.presentation and tonumber(call.presentation.duration_ms)
      local status, status_style = tool_status(call)
      local header = {
        left = {
          { text = "Bash", style = "tool_name" },
          { text = " · ", style = "dim" },
          { text = status, style = status_style },
        },
        right = duration and { { text = string.format("%.0f ms", duration), style = "dim" } } or {},
      }
      local body = {
        { kind = "code", text = command, language = "bash", syntax_highlight = true, line_numbers = false },
      }
      if type(args.workdir) == "string" and args.workdir ~= "" then
        body[#body + 1] = { spans = {
          { text = "in ", style = "dim" },
          { text = args.workdir, style = "code" },
        } }
      end
      if type(call.output) == "string" and call.output ~= "" then
        body[#body + 1] = { text = call.output, style = "tool_output" }
      end
      return { header = header, body = body }
    end },
    Glob = { render = function(call)
      local args = type(call.args) == "table" and call.args or {}
      local duration = call.presentation and tonumber(call.presentation.duration_ms)
      local status, status_style = tool_status(call)
      local body = {}
      if type(args.pattern) == "string" and args.pattern ~= "" then
        body[#body + 1] = { spans = {
          { text = "pattern ", style = "dim" },
          { text = args.pattern, style = "code" },
        } }
      end
      if type(args.path) == "string" and args.path ~= "" then
        body[#body + 1] = { spans = {
          { text = "in ", style = "dim" },
          { text = args.path, style = "code" },
        } }
      end
      if type(call.output) == "string" and call.output ~= "" then
        body[#body + 1] = { text = call.output, style = "tool_output" }
      end
      return {
        header = {
          left = {
            { text = "Glob", style = "tool_name" },
            { text = " · ", style = "dim" },
            { text = status, style = status_style },
          },
          right = duration and { { text = string.format("%.0f ms", duration), style = "dim" } } or {},
        },
        body = body,
      }
    end },
    Grep = { render = function(call)
      local args = type(call.args) == "table" and call.args or {}
      local duration = call.presentation and tonumber(call.presentation.duration_ms)
      local status, status_style = tool_status(call)
      local body = {}
      if type(args.pattern) == "string" and args.pattern ~= "" then
        body[#body + 1] = { spans = {
          { text = "pattern ", style = "dim" },
          { text = args.pattern, style = "code" },
        } }
      end
      if type(args.path) == "string" and args.path ~= "" then
        body[#body + 1] = { spans = {
          { text = "in ", style = "dim" },
          { text = args.path, style = "code" },
        } }
      end
      if type(args.glob) == "string" and args.glob ~= "" then
        body[#body + 1] = { spans = {
          { text = "files ", style = "dim" },
          { text = args.glob, style = "code" },
        } }
      end
      local mode = type(args.output_mode) == "string" and args.output_mode or "files_with_matches"
      if mode ~= "files_with_matches" or args.case_insensitive then
        body[#body + 1] = { text = mode .. (args.case_insensitive and " · case-insensitive" or ""), style = "dim" }
      end
      if type(call.output) == "string" and call.output ~= "" then
        body[#body + 1] = { text = call.output, style = "tool_output" }
      end
      return {
        header = {
          left = {
            { text = "Grep", style = "tool_name" },
            { text = " · ", style = "dim" },
            { text = status, style = status_style },
          },
          right = duration and { { text = string.format("%.0f ms", duration), style = "dim" } } or {},
        },
        body = body,
      }
    end },
    Edit = { render = function(call)
      if call.is_error then return nil end
      local a = call.args
      if type(a) ~= "table" then return nil end
      local lines = { { text = "Edit  " .. (a.path or ""), style = "tool_name" } }
      for _, side in ipairs({ { a.old_string, "- ", "removed" }, { a.new_string, "+ ", "added" } }) do
        local count = 0
        for line in ((side[1] or "") .. "\n"):gmatch("(.-)\n") do
          count = count + 1
          if count > 12 then
            lines[#lines + 1] = { text = "... more lines", style = "dim" }
            break
          end
          lines[#lines + 1] = { text = side[2] .. line, style = side[3] }
        end
      end
      return lines
    end },
    Read = { render = function(call)
      if call.is_error then return nil end
      local a = call.args
      if type(a) ~= "table" or type(a.path) ~= "string" or type(call.output) ~= "string" then return nil end
      -- Read returns cat -n output. Strip only its gutter, not source indentation,
      -- and keep notices outside the code block (also works for saved history).
      local source, notices, start = {}, {}, nil
      for line in call.output:gmatch("([^\n]*)\n?") do
        local number, text = line:match("^%s*(%d+)\t(.*)$")
        if number then
          start = start or tonumber(number)
          source[#source + 1] = text
        elseif line ~= "" then
          notices[#notices + 1] = { text = line, style = "dim" }
        end
      end
      local status, status_style = tool_status(call)
      if not start and status ~= "running" then return nil end
      local body = {}
      if start then
        body[#body + 1] = { kind = "code", text = table.concat(source, "\n") .. "\n", language = file_language(a.path),
          syntax_highlight = true, line_numbers = true, start_line = start }
      end
      for _, notice in ipairs(notices) do body[#body + 1] = notice end
      return {
        header = {
          left = { { text = "Read", style = "tool_name" }, { text = "  " .. a.path, style = "code" } },
          right = { { text = status, style = status_style } },
        },
        body = body,
      }
    end },
    Write = { render = function(call)
      if call.is_error then return nil end
      local a = call.args
      if type(a) ~= "table" or type(a.path) ~= "string" or type(a.content) ~= "string" then return nil end
      local status, status_style = tool_status(call)
      return {
        header = {
          left = {
            { text = "Write", style = "tool_name" },
            { text = "  " .. a.path, style = "code" },
          },
          right = { { text = status, style = status_style } },
        },
        body = {
          { kind = "code", text = a.content, language = file_language(a.path), syntax_highlight = true, line_numbers = true },
        },
      }
    end },
  },
}
-- This styles supported surfaces; it does not alter the terminal palette/font.
