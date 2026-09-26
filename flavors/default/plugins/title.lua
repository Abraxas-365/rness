-- /title [text | auto | unpin] — view, rename, or regenerate the session title.
--
-- A title set here pins: automatic titles (plugins/session-title.lua) never
-- overwrite it until `/title unpin`. `/title auto` generates a model title
-- from the first prompt now and pins it.

return function(opts)
  opts = opts or {}
  local request = { profile = opts.profile, timeout = opts.timeout }

  rness.commands.register {
    name = "title",
    description = "View, rename, or auto-generate session title",
    usage = "[text | auto | unpin]",
    run = function(ctx)
      local text = ctx.raw_input:match("^%s*(.-)%s*$")

      if text == "" then
        local info = rness.session.title_info(ctx.session)
        if not info then
          return { message = "(no title)" }
        end
        local pinned = info.source == "user" and " (pinned)" or ""
        return { message = info.title .. pinned }
      end

      if text == "auto" then
        local ok, result = pcall(rness.session.generate_title, ctx.session, request)
        if not ok then
          return { message = "Title generation failed: " .. tostring(result) }
        end
        return { message = "Title: " .. rness.session.title(ctx.session, result) }
      end

      if text == "unpin" then
        local info = rness.session.title_info(ctx.session)
        if not info or info.source ~= "user" then
          return { message = "Title is not pinned" }
        end
        rness.session.title(ctx.session, info.title, "model")
        return { message = "Title unpinned" }
      end

      local title = rness.session.title(ctx.session, text)
      return { message = "Title set: " .. title }
    end,
  }
end
