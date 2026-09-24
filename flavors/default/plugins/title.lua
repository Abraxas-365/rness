-- /title [text] — view or set the current session's title.
rness.commands.register {
  name = "title",
  description = "View or rename the current session",
  usage = "[new title]",
  run = function(ctx)
    local text = ctx.raw_input:match("^%s*(.-)%s*$")
    if text == "" then
      local title = rness.session.title(ctx.session)
      return { message = title or "(no title)" }
    end
    rness.session.title(ctx.session, text)
    return { message = "Title set: " .. text }
  end,
}
