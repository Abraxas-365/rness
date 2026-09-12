-- Add { name = "plan", file = "plugins/plan.lua" } to init.lua's setup list.
-- Opt-in plan collaboration. Questions must have an available frontend to review.
rness.plan.enable()
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
    if state.pending ~= nil then message = message .. (state.pending and "; activation pending next step" or "; exit pending next step") end
    return { message = message, data = state }
  end,
}
