-- Default flavor. Startup policy is explicit; edit these modules freely.
require("providers")
require("theme")
require("agents")

-- Set mode to "both" or "ptc" to enable isolated Lua run_code programs.
-- Add exact tool names to deferred to load their schemas through ToolSearch.
rness.tool_exposure = { mode = "native", deferred = {} }

-- Keyless search and anonymous fetch are independent; remove either to disable it.
-- DuckDuckGo uses HTML search and may be rate-limited. No silent backend fallback.
rness.web = {
  search = { provider = "duckduckgo" },
  fetch = { max_response_bytes = 5000000, max_body_chars = 100000, timeout_ms = 30000, max_redirects = 5 },
  -- search = { provider = "exa", api_key_env = "EXA_API_KEY" },
  -- search = { provider = "perplexity", api_key_env = "PERPLEXITY_API_KEY", model = "sonar" },
  -- search = { provider = "deepseek", api_key_env = "DEEPSEEK_API_KEY", model = "deepseek-v4-flash" },
}

rness.plugins.setup({
  { name = "references", file = "plugins/references.lua", opts = { max_results = 20 } },
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
