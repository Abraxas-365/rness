-- Default flavor. Startup policy is explicit; edit these modules freely.
require("providers")
require("theme")
require("agents")
require("system_prompt")

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

-- Output quotas and age-based cleanup are off in the core defaults.
-- This flavor opts in; set any byte/age limit to 0 to disable it.
rness.jobs.setup {
  retention = {
    max_job_bytes = 256 * 1024 * 1024,
    max_total_bytes = 2 * 1024 * 1024 * 1024,
    max_age_secs = 7 * 24 * 60 * 60,
    cleanup_interval_secs = 60,
  },
}

-- Shared image admission and normalization policy (also used by read_image).
rness.images = {
  max_input_bytes = 20 * 1024 * 1024,
  max_input_pixels = 64000000,
  max_input_dimension = 8192,
  max_request_images = 20,
  max_request_bytes = 200 * 1024 * 1024, -- encoded request budget, not source bytes
  max_pixels = 2048 * 2048,
  max_dimension = 8192,
  max_bytes = 4 * 1024 * 1024, -- hard normalized-byte limit
  animation = "first_frame",
  normalize_srgb = true,
  lossless = false,
  quality = 85,
}

rness.plugins.setup({
  { name = "read-image", file = "plugins/read-image.lua" },
  { name = "delivery", file = "plugins/delivery.lua", keys = { queue = "<F8>", steer = "<F9>" } },
  { name = "references", file = "plugins/references.lua", opts = { max_results = 20, allow_parent = false, allow_home = false, allow_absolute = false } },
  { name = "statusline", file = "plugins/statusline.lua" },
  { name = "jobs", file = "plugins/jobs.lua" },
  { name = "terminals", file = "plugins/terminals.lua" },
  { name = "sessions", file = "plugins/sessions.lua" },
  -- Optional session-history search: uncomment BOTH entries to enable all five tools.
  -- SQLite indexing is lazy; no index is opened until the first search.
  -- { name = "session-search-sqlite", file = "plugins/session-search-sqlite.lua" },
  -- { name = "session-search", file = "plugins/session-search.lua", dependencies = { "session-search-sqlite" } },
  { name = "branches", file = "plugins/branches.lua" },
  { name = "tasks", file = "plugins/tasks.lua" },
  { name = "questions", file = "plugins/questions.lua" },
  { name = "plan", file = "plugins/plan.lua", dependencies = { "questions" } },
  -- Optional context injection: give the model awareness of current time and
  -- tmux location. Useful for long-running sessions, scheduled tasks, or when
  -- the model needs to reason about time gaps between messages. Each injects a
  -- short message before model calls via pre_step hooks. Configure throttling
  -- with rness.time_context / rness.tmux_context tables above plugins.setup.
  -- { name = "time-context", file = "plugins/time-context.lua" },
  -- { name = "tmux-context", file = "plugins/tmux-context.lua" },
  -- Optional reminders: schedule_create/list/delete tools let the model set
  -- one-shot or recurring reminders ("every 10 minutes check the deploy").
  -- A due reminder wakes the idle session and starts a turn. Enable it when you
  -- keep rness open and want the agent to follow up on its own. Only fires
  -- while rness is running. If a turn is running when one comes due, it waits
  -- for that turn to end (delivery = "queue", default) or joins it at the next
  -- step (delivery = "steer"): rness.schedule = { delivery = "steer" }.
  -- { name = "schedule", file = "plugins/schedule.lua" },
  { name = "commands", file = "plugins/commands.lua" },
  { name = "title", file = "plugins/title.lua" },
  { name = "agent-controls", file = "plugins/agent-controls.lua" },
  { name = "models", file = "plugins/models.lua" },
})
rness.keymap.setup({})
rness.ui.promptbox = { paste = { keys = { preview = "alt+g" } } }
-- Global policy; explicit provider/model entries override this default.
-- Token counts are estimates. Use lower thresholds for smaller context windows.
rness.compaction = {
  default = {
    threshold_tokens = 165000, retain_tokens = 24000, summary_tokens = 4096,
    -- summary_profile = "compactor", -- optional declared profile; otherwise current session model
    system_prompt = [[You are a compaction engine for an AI coding assistant. Output only the requested checkpoint text. Do not call any tool or take any other action.]],
    prompt = [[You are now acting as a compaction engine for this AI coding assistant. Condense the conversation ABOVE into a structured checkpoint that lets another model resume the work with no loss of essential context.

Output EXACTLY the Markdown structure below: keep every section, in order. Use terse bullets, not prose paragraphs. Write "(none)" for an empty section — never drop a section.

## Primary Request and Intent
- [the user's original and evolving goals; quote verbatim where the exact wording matters]

## Key Technical Concepts
- [technologies, frameworks, patterns, and conventions in play]

## Files and Code
- [exact path: why it matters, key changes or snippets]

## Errors and Fixes
- [error: how it was resolved, plus any related user feedback]

## Pending Jobs
- [explicitly requested work not yet completed]

## Current Work
- [precisely what was in progress at this checkpoint]

## Next Step
- [the single next action, directly in line with the most recent request, or "(none)"]

## Critical Context
- [decisions and their rationale, constraints, user preferences, open questions, data needed to continue]

Rules:
- Write concise English engineering prose. Preserve exact file paths, commands, error strings, identifiers, numeric values, function signatures, and syntax fragments.
- Capture user feedback and explicit instructions faithfully, especially corrections.
- Do NOT mention this summarization request or that the context was compacted.
- Output only the checkpoint text: do not call any tool or take any other action.
- If the conversation already contains a <compacted-summary> block, it is a PRIOR checkpoint. Do not copy it forward verbatim: preserve still-true facts, drop stale ones, and merge newer information into a single consolidated summary under the same structure.]],
    max_overflow_retries = 1, max_compactions = 2,
    prune_threshold = 8192, prune_head = 4096, prune_tail = 1024,
  },
}
