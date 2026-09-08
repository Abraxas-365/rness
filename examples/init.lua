-- Single entry point. Copy to ~/.rness/init.lua and edit.
-- This file is NOT loaded from examples/. Lua changes currently require restart.
-- Register providers, profiles, hooks and UI here. Hooks run after startup.
-- Optional modules: require('providers') loads ~/.rness/lua/providers.lua.
-- Plugins are opt-in: selected files execute after engine mount, in this order.
-- Copy each desired example into ~/.rness/plugins/ before uncommenting:
-- rness.plugins.load("spinner")
-- rness.plugins.load("sessions")
-- Files in plugins/ are never activated automatically.
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
