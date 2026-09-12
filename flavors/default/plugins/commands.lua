rness.commands.register {
  name = "compact",
  description = "Summarize the current context now (makes a model request)",
  run = function(ctx)
    assert(ctx.raw_input:match("^%s*$"), "Usage: /compact")
    local view = rness.session.compaction_view(ctx.session)
    if #view.messages == 0 then return { message = "Nothing to compact." } end
    local model = rness.session.config(ctx.session).selection
    local policies = rness.compaction or {}
    local policy = (model and policies[model.route .. "/" .. model.model]) or policies.default
    assert(policy, "Configure a compaction policy first")
    local changed = rness.session.compact_region(ctx.session, {
      start = 1, ["end"] = #view.messages, sources = view.sources, policy = policy,
    })
    return { message = changed and "Context compacted." or "No smaller summary produced; history unchanged." }
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
