-- Add { name = "commands", file = "plugins/commands.lua" } to init.lua's
-- single rness.plugins.setup list after copying this file.

-- Explicit review/confirmation prevents compaction of an unseen or stale range.
local regions = {}
local picker = {}
rness.ui.app {
  name = "compact-region", slot = "overlay", title = "Compact region", keymap = "ctrl+g",
  key_help = { "j/k: move", "space: anchor", "enter: review", "r: refresh", "esc: close" },
  view = function(ctx)
    local state = picker[ctx.session]
    if not state then
      state = { view = rness.session.compaction_view(ctx.session), cursor = 1, anchor = 1 }
      picker[ctx.session] = state
    end
    local rows = math.max(1, (ctx.rows or 16) - 2)
    local first = math.max(1, state.cursor - math.floor(rows / 2))
    local lines = { "j/k move | space anchor | enter review | r refresh", state.notice or "No request is sent from this picker." }
    for i = first, math.min(#state.view.messages, first + rows - 1) do
      local kind, body = next(state.view.messages[i])
      local preview = rness.json.encode(body):gsub("%c", " "):sub(1, 120)
      local selected = i >= math.min(state.anchor, state.cursor) and i <= math.max(state.anchor, state.cursor)
      lines[#lines + 1] = string.format("%s%s %d %s %s", i == state.cursor and ">" or " ", selected and "*" or " ", i, kind, preview)
    end
    if #state.view.messages == 0 then lines[#lines + 1] = "No model messages. Press r to refresh after a turn." end
    while #lines < math.min(18, math.max(3, #state.view.messages + 2)) do lines[#lines + 1] = "" end
    return lines
  end,
  on_key = function(key, ctx)
    local state = picker[ctx.session]
    if not state then return true end
    if key == "r" then
      picker[ctx.session] = nil
      regions[ctx.session] = nil
    elseif key == "j" or key == "down" then
      state.cursor = math.max(1, math.min(#state.view.messages, state.cursor + 1))
    elseif key == "k" or key == "up" then
      state.cursor = math.max(1, state.cursor - 1)
    elseif key == "space" or key == " " then
      state.anchor = state.cursor
    elseif key == "enter" then
      local first, last = math.min(state.anchor, state.cursor), math.max(state.anchor, state.cursor)
      local config = rness.session.config(ctx.session)
      local model = config.selection
      local policy = model and rness.compaction and rness.compaction[model.route .. "/" .. model.model]
      if not policy or first < 1 or last > #state.view.messages or last - first >= 100 then
        state.notice = "Configure a route policy and select 1-100 messages."
      else
        regions[ctx.session] = { start = first, ["end"] = last, sources = state.view.sources, policy = policy }
        state.notice = string.format("Range %d-%d: paid request. Esc, then /compact-region confirm or cancel.", first, last)
      end
    else
      return false
    end
    return true
  end,
}
rness.commands.register {
  name = "compact-region",
  description = "Preview model-message ranges and explicitly compact a selected range",
  usage = "[start end|confirm|cancel]",
  arguments = { "confirm", "cancel" },
  run = function(ctx)
    local argument = ctx.raw_input:match("^%s*(.-)%s*$")
    if argument == "cancel" then
      regions[ctx.session] = nil
      return { message = "Region selection cancelled." }
    end
    if argument == "confirm" then
      local selection = regions[ctx.session]
      assert(selection, "Preview a range with /compact-region start end first")
      regions[ctx.session] = nil
      local changed = rness.session.compact_region(ctx.session, selection)
      return { message = changed and "Selected region compacted." or "No smaller summary produced; history unchanged." }
    end
    local view = rness.session.compaction_view(ctx.session)
    local first, last = argument:match("^(%d+)%s+(%d+)$")
    first, last = tonumber(first), tonumber(last)
    if argument ~= "" then
      assert(first and last and first >= 1 and last >= first and last <= #view.messages,
        "Usage: /compact-region [start end|confirm|cancel]; indices are one-based inclusive")
      assert(last - first < 100, "Select at most 100 messages so the complete range can be previewed")
      local config = rness.session.config(ctx.session)
      local model = config.selection
      local policy = model and rness.compaction and rness.compaction[model.route .. "/" .. model.model]
      assert(policy, "Configure rness.compaction for this session's provider/model first")
      regions[ctx.session] = { start = first, ["end"] = last, sources = view.sources, policy = policy }
    else
      regions[ctx.session] = nil
    end
    local lines = {}
    for index = first or 1, last or #view.messages do
      local message = view.messages[index]
      local kind, body = next(message)
      local preview = rness.json.encode(body):gsub("%c", " ")
      if #preview > 160 then preview = preview:sub(1, 160) .. "..." end
      lines[#lines + 1] = string.format("%d %s %s", index, kind, preview)
      if #lines == 100 then
        lines[#lines + 1] = "Preview limited to 100 messages; select a narrower range to inspect the remainder."
        break
      end
    end
    if first then
      lines[#lines + 1] = "This makes a paid summarization request. Originals remain in the log."
      lines[#lines + 1] = "Run /compact-region confirm to apply, or /compact-region cancel."
    else
      lines[#lines + 1] = "Select with /compact-region start end. Tool-call/result pairs must stay together."
    end
    return { message = table.concat(lines, "\n") }
  end,
}
rness.commands.register {
  name = "project",
  description = "Show the current session project",
  usage = "[path|session]",
  arguments = { "path", "session" },
  run = function(ctx)
    local argument = ctx.raw_input:match("^%s*(.-)%s*$")
    if argument == "session" then
      return { message = ctx.session, data = { session = ctx.session } }
    end
    if argument ~= "" and argument ~= "path" then
      error("Usage: /project [path|session]")
    end
    return {
      message = ctx.workspace or "No session workspace",
      data = { workspace = ctx.workspace },
    }
  end,
}
