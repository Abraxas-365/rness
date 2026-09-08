-- Copy to ~/.rness/lua/roles.lua; require("roles") in init.lua.
-- Profiles are omitted deliberately: roles reuse the session's model settings.
rness.agents.declare("planner", {
  description = "Discusses a plan without invoking tools.",
  instructions = "Clarify requirements, explain alternatives, and produce a scoped plan.",
  subagent = false,
  tools = {},
})
rness.agents.declare("implementer", {
  description = "Implements a bounded task and verifies the result.",
  instructions = "Follow repository conventions, avoid unrelated changes, and report actual test results.",
  subagent = true,
})
-- A tool-free principal cannot delegate through the subagent tool.
-- Select implementer or an unrestricted principal when delegation is needed.
