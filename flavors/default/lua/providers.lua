-- Connections do not authenticate or send requests until selected.
rness.providers.register("anthropic", {
  protocol = "anthropic", base_url = "https://api.anthropic.com",
  auth = { env = "ANTHROPIC_API_KEY" },
})
rness.providers.register("chatgpt", {
  protocol = "chatgpt-responses", base_url = "https://chatgpt.com/backend-api",
  auth = { oauth = "openai-chatgpt" },
})
rness.providers.register("openrouter", {
  protocol = "openai-chat", base_url = "https://openrouter.ai/api/v1",
  auth = { env = "OPENROUTER_API_KEY" },
})
rness.providers.register("ollama", {
  protocol = "openai-chat", base_url = "http://localhost:11434/v1", auth = false,
})
-- Resolve against the parent's current connection when delegating.
-- No cross-provider fallback: add an available model for each connection you use.
rness.profiles.declare("small", {
  by_provider = {
    anthropic = {
      model = "claude-haiku-4-5-20251001",
    },
    -- openrouter = { model = "your-model-id" },
    -- ollama = { model = "your-installed-model" },
    chatgpt = { model = "gpt-5.6-luna", options = { reasoning = { kind = "effort", effort = "high" } } },
  },
})
-- No principal model is selected silently. Start with --model provider/model,
-- or declare a principal profile and set rness.default_profile yourself.
