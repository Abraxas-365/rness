-- User controls include one-shot and continuable descendants. These commands
-- never write the parent's conversation and are safe while its turn is running.
local function running(session)
  local agents = {}
  for _, child in ipairs(rness.subagents.list(session)) do
    if child.running then agents[#agents + 1] = child end
  end
  return agents
end

local function listing(agents)
  if #agents == 0 then return "No running subagents." end
  local lines = { "Running subagents (session IDs):" }
  for _, child in ipairs(agents) do
    lines[#lines + 1] = child.session .. "  parent=" .. child.parent .. "  depth=" .. child.depth
  end
  lines[#lines + 1] = "Use /agents stop <id> to stop one current turn."
  return table.concat(lines, "\n")
end

rness.commands.register {
  name = "agents",
  description = "List running subagents or stop a child turn",
  usage = "[stop [id]]",
  arguments = { "stop" },
  allow_busy = true,
  complete = function(ctx)
    local choices = { "stop" }
    for _, child in ipairs(running(ctx.session)) do
      choices[#choices + 1] = "stop " .. child.session
    end
    return choices
  end,
  run = function(ctx)
    local input = ctx.raw_input:match("^%s*(.-)%s*$")
    local agents = running(ctx.session)
    if input == "" then return { message = listing(agents), data = agents } end
    local id = input:match("^stop%s+(%S+)$")
    assert(input == "stop" or id, "Usage: /agents [stop [id]]")
    if not id then
      if #agents ~= 1 then return { message = listing(agents), data = agents } end
      id = agents[1].session
    end
    local found = false
    for _, child in ipairs(agents) do
      if child.session == id then found = true; break end
    end
    assert(found, "Not a running subagent of this session: " .. id .. ". Use /agents to refresh.")
    rness.subagents.interrupt(ctx.session, id)
    return { message = "Stop requested for subagent " .. id, data = { session = id } }
  end,
}
