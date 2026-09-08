-- Copy into ~/.rness/plugins/commands.lua and load explicitly from init.lua.
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
