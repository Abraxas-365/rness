-- /title [text|auto] — view, set, or auto-generate the session title.
--
-- Configure LLM title generation via init.lua:
--   rness.title_gen = {
--     profile = "title-gen",  -- optional; defaults to session model
--     timeout = 15,           -- seconds; default 15
--   }

local config = rness.title_gen or {}

rness.commands.register {
  name = "title",
  description = "View, rename, or auto-generate session title",
  usage = "[text | auto]",
  run = function(ctx)
    local text = ctx.raw_input:match("^%s*(.-)%s*$")

    -- /title — show current
    if text == "" then
      local title = rness.session.title(ctx.session)
      return { message = title or "(no title)" }
    end

    -- /title auto — generate via LLM
    if text == "auto" then
      local existing = rness.session.title(ctx.session) or "New session"
      local title = rness.llm.complete {
        system = "Create a concise title for an AI coding-assistant session from "
          .. "the supplied human message. Return only the title on one line, in "
          .. "plain text of natural language, no quotes, no prefix, no markdown, "
          .. "no code. Use the language of the message. Aim for 4-8 words.",
        prompt = existing,
        profile = config.profile,
        timeout = config.timeout or 15,
      }
      if title and #title > 0 and #title <= 200 then
        rness.session.title(ctx.session, title)
        return { message = "Title: " .. title }
      end
      return { message = "LLM title generation failed" }
    end

    -- /title <text> — manual set
    rness.session.title(ctx.session, text)
    return { message = "Title set: " .. text }
  end,
}
