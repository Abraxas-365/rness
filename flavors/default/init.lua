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
rness.ui.promptbox = { paste = { keys = { preview = "alt+g" } } }
-- Global policy; explicit provider/model entries override this default.
-- Token counts are estimates. Use lower thresholds for smaller context windows.
rness.compaction = {
  default = {
    threshold_tokens = 165000, retain_tokens = 24000, summary_tokens = 4096,
    max_overflow_retries = 1, max_compactions = 2,
    prune_threshold = 8192, prune_head = 4096, prune_tail = 1024,
  },
}
