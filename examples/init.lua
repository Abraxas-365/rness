-- Single entry point. Copy to ~/.rness/init.lua and edit.
-- This file is NOT loaded from examples/. Restart after changing startup config.
-- Register providers, profiles, hooks and UI here. Hooks run after startup.
-- Optional modules: require('providers') loads ~/.rness/lua/providers.lua.
-- Plugins are opt-in: setup runs after engine mount, with dependencies first.
-- Paths below are relative to the directory containing init.lua; plugins/ is
-- only a suggested location. Copy selected files there before enabling them.
-- Use ONE setup list. Copying or installing a plugin never activates it.
rness.plugins.setup({
  -- { name = "spinner", file = "plugins/spinner.lua", watch = true },
  -- { name = "sessions", file = "plugins/sessions.lua", watch = true },
  -- { name = "text-tools", file = "plugins/text-tools.lua" },
  -- { name = "keymaps", file = "plugins/keymaps.lua",
  --   opts = { text = "Review the current changes." },
  --   keys = { insert_review = { "<F6>", "<F7>" } } },
  -- Use keys = { insert_review = false } to disable one slot, or keys = false
  -- to disable all defaults declared by plugin.keys (not legacy app handlers).
  -- Installed package: identity comes from its manifest; do not add name.
  -- { package = "my-plugin" },
  -- Inline setup runs at the same deferred stage as file/package setup.
  -- { name = "references", config = function(opts, plugin)
  --     rness.file_references.enable(opts)
  --   end, opts = { max_results = 20 } },
})
-- watch = true reloads selected files; init.lua/startup modules require restart.
-- Managed Git packages cannot be watched; linked development packages can.
-- Plain registration chunks remain valid with empty opts; options require a
-- returned setup function. See plugins/keymaps.lua for actions and stable slots.

-- Optional Bash filesystem sandbox (disabled unless you opt in).
-- macOS only for now; restricted modes fail closed on unsupported hosts.
-- Approval is independent; this does not confine plugins or non-Bash tools.
-- rness.sandbox.setup({
--   default = "workspace-write", agent_overrides = "tighten-only", unavailable = "deny",
-- })

-- Central mappings are startup-only. Add entries to this ONE mapping list.
rness.keymap.setup({
  -- { scope = "global", key = "ctrl+k", action = "core.scroll_up" },
  -- { scope = "global", key = "ctrl+j", action = "core.scroll_down" },
  -- { scope = "promptbox", key = "enter", action = "core.promptbox.noop" },
  -- { scope = "promptbox", key = "f8", action = "core.promptbox.submit" },
  -- { scope = "app:sessions", key = "f10", action = "core.app.close" },
})
-- Automatic boundary compaction is opt-in, configured here (restart required).
-- Use verified model budgets; these numbers are illustrative, not capabilities.
-- Do not also enable the legacy turn-end autocompact plugin for the same route.
-- rness.compaction = {
--   ["ollama/qwen3:14b"] = {
--     threshold_tokens = 24000, retain_tokens = 4800, summary_tokens = 2048,
--     max_overflow_retries = 1, max_compactions = 2,
--     prune_threshold = 8192, prune_head = 4096, prune_tail = 1024,
--   },
-- }
-- Budgets use heuristic request estimates, not exact provider token counts.
-- System/tool definitions are included in pressure but cannot be compacted.

-- /help bindings shows effective bindings; focused editors/modals capture input.
-- Engine calls during startup belong inside rness.hook.on('ready', function() ... end).
-- No credentials are stored here: auth references environment or credential store.

rness.providers.register("ollama", {
  protocol = "openai-chat",
  base_url = "http://localhost:11434/v1",
  auth = false,
})

rness.providers.register("router", {
  protocol = "openai-chat",
  base_url = "https://openrouter.ai/api/v1",
  auth = { env = "OPENROUTER_API_KEY" },
})

rness.providers.register("groq", {
  protocol = "openai-chat",
  base_url = "https://api.groq.com/openai/v1",
  auth = { env = "GROQ_API_KEY" },
})

rness.providers.register("anthropic", {
  protocol = "anthropic",
  base_url = "https://api.anthropic.com",
  auth = { env = "ANTHROPIC_API_KEY" },
  -- Instead of env, choose ONE of these:
  -- auth = { credential = "anthropic" }, -- stored API key only
  -- auth = { oauth = "anthropic" },      -- stored OAuth tokens, with refresh
})

-- ChatGPT subscription transport: OAuth only (not OpenAI API keys).
-- Uncomment after authenticating the corresponding credential-store entry.
-- rness.providers.register("chatgpt", {
--   protocol = "chatgpt-responses",
--   base_url = "https://chatgpt.com/backend-api/codex",
--   auth = { oauth = "openai-chatgpt" },
-- })

-- Profiles are optional saved selections. Model IDs must exist at the endpoint.
-- Replace these examples with IDs from your installed models/provider catalog.
rness.profiles.declare("local-qwen", {
  provider = "ollama",
  model = "qwen3:14b",
})

rness.profiles.declare("router-sonnet", {
  provider = "router",
  model = "anthropic/claude-sonnet-4.5",
  -- Optional request preferences; check gateway/model support first:
  -- options = {
  --   reasoning = { kind = "effort", effort = "high" },
  --   max_output_tokens = 8000,
  -- },
})

-- Explicit personal default, if desired. Otherwise select at the CLI.
-- rness.default_profile = "local-qwen"

-- Capabilities are facts, NOT request preferences. Unknown is allowed.
-- Declare only verified limits for this connection + model; no sample
-- limits are enabled here because local server settings/gateways can differ.
-- rness.models.declare {
--   provider = "ollama",
--   model = "qwen3:14b",
--   capabilities = {
--     context_window = YOUR_VERIFIED_CONTEXT_LIMIT,
--     max_output_tokens = YOUR_VERIFIED_OUTPUT_LIMIT,
--   },
-- }

-- Roles are selectable as the principal agent; delegation requires opt-in.
-- You can move these declarations to ~/.rness/lua/agents.lua and require('agents').
-- For scout, planner, worker, reviewer, researcher, context-builder, oracle,
-- and delegate examples, copy examples/lua/roles.lua to ~/.rness/lua/roles.lua.
-- Remove the worker declaration below before enabling require("roles").
-- Prefer spawn + a focused prompt for exploration; profiles can select a cheaper model.
-- require("roles")
rness.agents.declare("architect", {
  description = "Designs architecture with the user.",
  instructions = "Discuss requirements and tradeoffs before proposing implementation.",
  subagent = false, -- also the default when omitted
})

rness.agents.declare("worker", {
  description = "Implements a scoped task and verifies it with tests.",
  instructions = "Follow project conventions. Implement the assigned task and report verification results.",
  subagent = true,
  -- profile = "local-qwen", -- optional; otherwise inherit generation settings
  -- tools = {}, -- optional allowlist; empty means no tools
})

-- rness.default_agent = "architect"
-- Model tool call: subagent({provider="spawn", agent="worker", prompt="..."})
-- Lua after mount: rness.subagents.start("spawn", {parent=id, agent="worker", prompt="..."})
-- rness.subagents.roster() lists only delegable names and descriptions.
-- Children cannot exceed the parent's effective tool permissions.

-- Usage:
--   rness --agent architect --profile local-qwen
--   rness --profile local-qwen
--   rness --profile router-sonnet
--   rness --provider router --model anthropic/claude-sonnet-4.5
--   rness -s SESSION_ID  -- restores saved selection; connections stay here
