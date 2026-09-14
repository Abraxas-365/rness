-- Select the questions plugin, then add this entry to init.lua's setup list:
-- { name = "plan", file = "plugins/plan.lua", dependencies = { "questions" } }
-- Dependencies are selected/enabled plugin names, not paths; they load first.
-- Missing/disabled dependencies and cycles are rejected; unload Plan before Questions.
-- Opt-in plan collaboration. The headless check still requires an available Questions frontend.
rness.plan.enable {
  review = {
    title = "Plan review", width = 100, height = 30, border = "rounded",
    edit_enabled = true, approve_after_edit = true,
    -- editor = { "nvim" }, -- otherwise use VISUAL, then EDITOR
    -- GUI editors must wait: editor = { "code", "--wait" }
    keys = { approve = "a", feedback = "r", edit = "e" },
  },
}
rness.commands.register {
  name = "plan",
  description = "Select planning mode for the next step",
  usage = "[on|off|status]",
  arguments = { "on", "off", "status" },
  run = function(ctx)
    local arg = ctx.raw_input:match("^%s*(.-)%s*$")
    if arg == "" or arg == "on" then rness.session.plan(ctx.session, true)
    elseif arg == "off" then rness.session.plan(ctx.session, false)
    elseif arg ~= "status" then error("Usage: /plan [on|off|status]") end
    local state = rness.session.plan(ctx.session)
    local message = state.active and "Plan mode active" or "Plan mode inactive"
    if state.pending ~= nil then
      message = state.pending and "Plan mode will activate on the next AI step" or "Plan mode will deactivate on the next AI step"
    end
    return { message = message, data = state }
  end,
}
