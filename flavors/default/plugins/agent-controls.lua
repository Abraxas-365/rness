-- User controls include one-shot and continuable descendants, even while busy.
-- Aliases are assigned by the runtime, never by a filtered list position.
local usage = "Usage: /agents [<id> [steer <message>|stop [confirm]]] or /agents stop [<id>]"

local function display_id(child)
  return child.alias or child.session
end

local function hint(child)
  local role = type(child.name) == "string" and child.name or "subagent"
  local task = type(child.task) == "string" and child.task:gsub("%s+", " "):match("^%s*(.-)%s*$") or ""
  -- Bound the preview by Unicode characters, not bytes.
  local cut = utf8.offset(task, 121)
  if cut then task = task:sub(1, cut - 1) .. "…" end
  return role .. (task ~= "" and (" · " .. task) or "")
end

local function choice(value, description)
  -- Installed plugins may be loaded by an older binary.
  if rness.commands.completion_descriptions then
    return { value = value, description = description or "" }
  end
  return value
end

local function resolve(agents, id)
  for _, child in ipairs(agents) do
    if child.session == id or child.alias == id then return child end
  end
  error("Not a subagent of this session: " .. id .. ". Use /agents to refresh.")
end

local function running(agents)
  local result = {}
  for _, child in ipairs(agents) do
    if child.running then result[#result + 1] = child end
  end
  return result
end

local function listing(agents)
  if #agents == 0 then return "No running subagents." end
  local lines = { "Running subagents:" }
  for _, child in ipairs(agents) do
    lines[#lines + 1] = display_id(child) .. "  depth=" .. child.depth
  end
  lines[#lines + 1] = "Use /agents <id> stop to stop one current turn."
  return table.concat(lines, "\n")
end

local function open(ctx, child)
  return { data = { action = "agents:open", session = ctx.session,
    agent = child and child.session or nil } }
end

rness.commands.register {
  name = "agents",
  description = "Open the agent monitor, steer a child, or stop its current turn",
  usage = "[<id> [steer <message>|stop [confirm]]] | stop [<id>]",
  arguments = { "stop" },
  allow_busy = true,
  complete = function(ctx)
    local agents = rness.subagents.list(ctx.session, true)
    local raw = ctx.raw_input or ""
    local input = raw:match("^%s*(.-)%s*$")
    local inspect = input == ""
    local description = "Open agent monitor"
    for _, child in ipairs(agents) do
      if input == child.alias or input == child.session then
        inspect, description = true, hint(child)
      end
    end
    local choices = {}
    -- The host prepends "agents ". Preserve the exact inspect choice before
    -- verb suggestions, including trailing spaces used by its prefix filter.
    if inspect then choices[#choices + 1] = choice(raw:gsub("^%s", "", 1), description) end
    choices[#choices + 1] = choice("stop", "Choose a running agent to stop")
    for _, child in ipairs(agents) do
      -- One visible identity per child; full session IDs remain accepted input.
      local id, detail = display_id(child), hint(child)
      choices[#choices + 1] = choice(id, detail)
      choices[#choices + 1] = choice(id .. " steer", detail)
      choices[#choices + 1] = choice(id .. " stop", detail)
      if child.running then choices[#choices + 1] = choice("stop " .. id, detail) end
    end
    return choices
  end,
  run = function(ctx)
    local input = ctx.raw_input:match("^%s*(.-)%s*$")
    local agents = rness.subagents.list(ctx.session)
    if input == "" then return open(ctx) end
    local id, rest = input:match("^(%S+)%s*(.*)$")
    local child
    local confirmed = false
    if id == "stop" then
      assert(rest == "" or not rest:find("%s"), usage)
      if rest == "" then
        local active = running(agents)
        if #active ~= 1 then return { message = listing(active), data = active } end
        child = active[1]
      else
        child = resolve(agents, rest)
      end
    else
      child = resolve(agents, id)
      if rest == "" then return open(ctx, child) end
      local verb, text = rest:match("^(%S+)%s*(.*)$")
      if verb == "steer" then
        assert(text ~= "", "Usage: /agents <id> steer <message>")
        rness.subagents.steer_user(ctx.session, child.session, text)
        return { message = "User steering sent to subagent " .. display_id(child),
          data = { session = child.session } }
      end
      assert(verb == "stop" and (text == "" or text == "confirm"), usage)
      confirmed = text == "confirm"
    end
    -- Questions has no generic command-await API. Require an explicit command
    -- instead, pinning the suggested confirmation to the full durable ID.
    if not confirmed then
      return { message = "Stop only the current turn of subagent " .. display_id(child)
        .. "? Queued messages are not cleared; descendants keep running.\nConfirm: /agents "
        .. child.session .. " stop confirm", data = { session = child.session } }
    end
    rness.subagents.interrupt(ctx.session, child.session)
    return { message = "Stop requested for subagent " .. display_id(child),
      data = { session = child.session } }
  end,
}
