-- Add { name = "tasks", file = "plugins/tasks.lua" } to init.lua's setup list.
-- This app uses legacy keymap/on_key controls, not plugin.keys slots.
-- Rust owns durable tasks; Lua owns activation, layout, and navigation.
rness.tasks.enable { allow_parallel_in_progress = true }

rness.ui.messagebox.tool_card("TaskWrite", function(call)
  if call.is_error then return nil end
  local tasks = type(call.args) == "table" and call.args.tasks
  if type(tasks) ~= "table" then return nil end

  local completed, active, pending = 0, 0, 0
  for _, task in ipairs(tasks) do
    if task.status == "completed" then completed = completed + 1
    elseif task.status == "in_progress" then active = active + 1
    else pending = pending + 1 end
  end
  local total = #tasks
  local filled = total > 0 and math.floor(completed / total * 16) or 0
  local body = {
    { spans = {
      { text = "[" .. string.rep("=", filled) .. string.rep("-", 16 - filled) .. "]", style = "added" },
      { text = string.format("  %d/%d complete", completed, total), style = "dim" },
    } },
    { text = "" },
  }
  local markers = { pending = "[ ]", in_progress = "[>]", completed = "[x]" }
  local styles = { pending = "dim", in_progress = "heading", completed = "added" }
  for _, task in ipairs(tasks) do
    local style = styles[task.status] or "dim"
    body[#body + 1] = { spans = {
      { text = (markers[task.status] or "[ ]") .. "  ", style = style },
      { text = task.content, style = task.status == "in_progress" and "heading" or "dim" },
      { text = task.status == "in_progress" and "  <- in progress" or "", style = style },
    } }
  end
  if total == 0 then body[#body + 1] = { text = "No tasks yet.", style = "dim" } end
  return {
    header = {
      left = { { text = "Tasks", style = "tool_name" } },
      right = { { text = string.format("%d active · %d pending · %d done", active, pending, completed), style = "dim" } },
    },
    body = body,
  }
end)

local offsets = {}
rness.ui.app {
  name = "tasks",
  refresh_ms = 500, -- keep external session changes live while visible
  slot = "overlay",
  title = "Tasks | j/k PgUp/PgDn | esc",
  keymap = "ctrl+t",
  view = function(ctx)
    local tasks = rness.session.tasks(ctx.session).tasks
    local rows, cols = ctx.rows or 16, ctx.cols or 60
    if rows < 2 or cols < 8 then return { "Resize" } end
    local body = {}
    for _, task in ipairs(tasks) do
      local marker = ({pending = "[ ]", in_progress = "[>]", completed = "[x]"})[task.status]
      local wrapped = rness.ui.wrap(task.id .. ": " .. task.content, cols - 4)
      for i, line in ipairs(wrapped) do
        body[#body + 1] = (i == 1 and marker .. " " or "    ") .. line
      end
    end
    if #body == 0 then body[1] = "No saved tasks." end
    local count = rows - 1
    local offset = math.min(offsets[ctx.session] or 0, math.max(0, #body - count))
    offsets[ctx.session] = offset
    local lines = { string.format("%d tasks | %d-%d/%d", #tasks, offset + 1, math.min(#body, offset + count), #body) }
    for i = offset + 1, math.min(#body, offset + count) do lines[#lines + 1] = body[i] end
    -- External-app height follows the returned line count. Reserve the desired
    -- viewport independently of the current clipped terminal height.
    while #lines < math.min(28, #body + 1) do lines[#lines + 1] = "" end
    return lines
  end,
  on_key = function(key, ctx)
    local offset = offsets[ctx.session] or 0
    local page = math.max(1, (ctx.rows or 16) - 1)
    if key == "j" or key == "down" then offset = offset + 1
    elseif key == "k" or key == "up" then offset = offset - 1
    elseif key == "pagedown" then offset = offset + page
    elseif key == "pageup" then offset = offset - page
    elseif key == "home" then offset = 0
    elseif key == "end" then offset = math.maxinteger
    else return false end
    offsets[ctx.session] = math.max(0, offset)
    return true
  end,
}
