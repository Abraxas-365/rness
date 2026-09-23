-- Connections do not authenticate or send requests until selected.
rness.providers.register("anthropic", {
  protocol = "anthropic", base_url = "https://api.anthropic.com",
  auth = { env = "ANTHROPIC_API_KEY" },
})
-- Prompt caching is enabled by default for Anthropic routes. Use a 1-hour TTL
-- by default; override it with "5m" if lower API-key cache-write cost matters.
-- The TTL is latched on first use; changing it mid-session has no effect.
rness.providers.set_cache_ttl("anthropic", "1h")
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
-- Model metadata: OpenAI model pages and platform.claude.com/docs/en/models/overview.
-- These are declarations, not availability guarantees for your account.
for _, model in ipairs({ "gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna","gpt-6-sol", "gpt-6-luna" }) do
  rness.models.declare {
    provider = "chatgpt", model = model,
    capabilities = {
      context_window = 1050000, max_output_tokens = 128000, image_input = true,
      -- The subscription Responses endpoint accepts an explicit output cap.
      -- Omit it to let ChatGPT select the model default.
      output_token_limit = true,
      reasoning = { efforts = model == "gpt-6-astra"
        and { "low", "medium", "high", "xhigh", "max" }
        or { "none", "low", "medium", "high", "xhigh", "max" } },
    },
  }
end
for _, model in ipairs({ "claude-opus-5", "claude-fable-5", "claude-sonnet-5", "claude-opus-4-8","claude-opus-5-5"}) do
  rness.models.declare {
    provider = "anthropic", model = model,
    capabilities = {
      context_window = 1000000, max_output_tokens = 128000, image_input = true,
      output_token_limit = true, temperature = false,
      reasoning = { efforts = { "low", "medium", "high", "xhigh", "max" } },
    },
  }
end
rness.models.declare {
  provider = "anthropic", model = "claude-opus-4-6",
  capabilities = {
    context_window = 1000000, max_output_tokens = 128000, image_input = true,
    output_token_limit = true,
    -- Temperature depends on thinking mode; leave it hidden in the editor.
    reasoning = { efforts = { "low", "medium", "high", "max" },
      budget_tokens = { min = 1024, max = 127999 } },
  },
}
rness.models.declare {
  provider = "anthropic", model = "claude-haiku-4-5-20251001",
  capabilities = {
    context_window = 200000, max_output_tokens = 64000, image_input = true,
    output_token_limit = true,
    -- Manual thinking requires budget < the request's output limit.
    -- Temperature depends on thinking mode; leave it hidden in the editor.
    reasoning = { budget_tokens = { min = 1024, max = 63999 } },
  },
}

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
