-- Default flavor. Startup policy is explicit; edit these modules freely.
require("providers")
require("theme")
require("agents")

rness.plugins.setup({
  { name = "spinner", file = "plugins/spinner.lua" },
  { name = "sessions", file = "plugins/sessions.lua" },
  { name = "branches", file = "plugins/branches.lua" },
  { name = "tasks", file = "plugins/tasks.lua" },
  { name = "plan", file = "plugins/plan.lua" },
  { name = "commands", file = "plugins/commands.lua" },
})
rness.keymap.setup({})
-- Ctrl+G opens compaction selection, so paste preview uses Alt+G.
rness.ui.promptbox = { paste = { keys = { preview = "alt+g" } } }
-- Automatic compaction requires verified budgets for your selected model.
-- Do not enable the legacy turn-end autocompact alongside boundary compaction.
