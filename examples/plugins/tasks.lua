-- Add { name = "tasks", file = "plugins/tasks.lua" } to init.lua's setup list.
-- This app uses legacy keymap/on_key controls, not plugin.keys slots.
-- Rust owns durable tasks; Lua owns activation, layout, and navigation.
rness.tasks.enable { allow_parallel_in_progress = true }

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
