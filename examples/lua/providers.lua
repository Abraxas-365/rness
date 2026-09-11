-- Copy to ~/.rness/lua/providers.lua and add
-- require("providers") to init.lua. Restart after changes.
-- Keep only the connections you use. Do not also register the same names in
-- init.lua: provider names must be unique (examples/init.lua includes some).
-- Registration does not contact providers, install models, or choose a default.
-- No providers are registered implicitly. Names below are your connection names.
-- Supply credentials through environment variables or the credential store.

-- Anthropic subscription: log in with `rness auth login --provider anthropic`.
rness.providers.register("anthropic", {
  protocol = "anthropic",
  base_url = "https://api.anthropic.com",
  -- Reads the "anthropic" OAuth credentials saved by:
  -- rness auth login --provider anthropic
  -- Default store: ~/.rness/credentials.json (or $RNESS_HOME/credentials.json).
  auth = { oauth = "anthropic" },
  -- Instead of OAuth, choose ONE:
  -- auth = { credential = "anthropic" }, -- stored API key
  -- auth = { env = "ANTHROPIC_API_KEY" }, -- API key from environment
})

rness.providers.register("openai", {
  protocol = "openai-chat",
  base_url = "https://api.openai.com/v1",
  auth = { env = "OPENAI_API_KEY" },
  -- auth = { credential = "openai" }, -- stored API key instead
})

-- ChatGPT subscription: log in with `rness auth login --provider openai-chatgpt`.
-- The connection name is independent of the OAuth credential-store key.
rness.providers.register("chatgpt", {
  protocol = "chatgpt-responses",
  base_url = "https://chatgpt.com/backend-api",
  auth = { oauth = "openai-chatgpt" },
})

rness.providers.register("deepseek", {
  protocol = "openai-chat",
  base_url = "https://api.deepseek.com/v1",
  auth = { env = "DEEPSEEK_API_KEY" },
})

-- OpenCode Go: OpenAI-compatible Chat Completions models.
-- Examples: kimi-k2.7-code, glm-5.2, deepseek-v4-pro, mimo-v2.5.
rness.providers.register("opencode-go", {
  protocol = "openai-chat",
  base_url = "https://opencode.ai/zen/go/v1",
  auth = { env = "OPENCODE_GO_API_KEY" },
})

-- OpenCode Go: Anthropic-compatible Messages models, using the same API key.
-- Examples: minimax-m3, minimax-m2.7, qwen3.7-max, qwen3.7-plus.
-- The transport appends /messages; do not append it to base_url yourself.
rness.providers.register("opencode-go-anthropic", {
  protocol = "anthropic",
  base_url = "https://opencode.ai/zen/go/v1",
  auth = { env = "OPENCODE_GO_API_KEY" },
})

-- Ollama: start the server and install the desired model separately.
-- Uses its OpenAI-compatible endpoint, not the native /api/chat endpoint.
-- Local context/output limits depend on your server configuration.
rness.providers.register("ollama", {
  protocol = "openai-chat",
  base_url = "http://localhost:11434/v1",
  auth = false,
})

-- OpenRouter: model IDs can contain a slash; use --provider and --model.
rness.providers.register("openrouter", {
  protocol = "openai-chat",
  base_url = "https://openrouter.ai/api/v1",
  auth = { env = "OPENROUTER_API_KEY" },
})

-- Groq: choose an available model from your account's catalog.
rness.providers.register("groq", {
  protocol = "openai-chat",
  base_url = "https://api.groq.com/openai/v1",
  auth = { env = "GROQ_API_KEY" },
})

-- Examples (availability and tool support depend on the provider/model):
-- rness -m opencode-go/kimi-k2.7-code
-- rness -m opencode-go-anthropic/minimax-m3
-- rness -m ollama/qwen3:14b
-- rness --provider openrouter --model <publisher/model>
-- rness --provider groq --model <model-id>
--
-- Optional capabilities: require("models") separately, after reviewing limits.
-- Optional profiles belong in startup too; none is imposed by this module.
