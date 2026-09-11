-- Add { name = "keymaps", file = "plugins/keymaps.lua" } to the single
-- rness.plugins.setup list in init.lua after copying this file.
-- opts.text customizes the inserted prompt; keys.insert_review remaps the
-- stable slot to a chord/list, false disables it, and keys=false disables all
-- plugin defaults. Central core mappings belong in init.lua's keymap.setup,
-- not here: that API is startup-only. See examples/init.lua.

return function(opts, plugin)
  local text = opts.text or "Review the current changes."
  assert(type(text) == "string", "opts.text must be a string")

  plugin.action("insert_review", {
    scope = "promptbox",
    description = "Insert a review request without submitting",
    run = function(ctx)
      ctx.promptbox.insert(text)
    end,
  })
  plugin.keys({
    insert_review = { action = "insert_review", key = "<F6>" },
  })
end
