-- Model capability declarations, reviewed 2026-09-09.
-- Copy to ~/.rness/lua/models.lua and require("models") from init.lua.
-- This is a startup module, NOT a runtime plugin. Restart after changes.
-- This is metadata, not provider registration or authentication. Prefixes must
-- match your rness.providers.register names, not credential-store identifiers.
-- Keep all active models from the source setup, including older selected models;
-- the disabled kuck provider and obsolete example-only entries are excluded.
--
-- context_window: published total context capacity (input + output).
-- max_output_tokens: published output ceiling, NOT a default request size.
-- reasoning: documented efforts or manual thinking budget range.
-- Declarations validate explicit request options but do not choose defaults.
-- They do not register providers or guarantee account access.
-- Codex/ChatGPT may impose smaller limits than the public OpenAI API; override
-- these values for your account before relying on autocompact's threshold.
-- spinner.lua and autocompact.lua consume the same startup context_window
-- through rness.models.get; the engine validates explicit output/effort options.
--
-- Sources (static snapshot; no network requests when this plugin loads):
-- https://platform.claude.com/docs/en/models/overview
-- https://platform.claude.com/docs/en/build-with-claude/effort
-- https://models.dev/providers/anthropic/
-- https://developers.openai.com/api/docs/models
-- https://models.dev/models/openai/gpt-5.3-codex-spark
-- https://models.dev/providers/opencode-go/
-- https://opencode.ai/docs/go/#endpoints

-- Anthropic: protocol = "anthropic".
rness.models.declare {
  provider = "anthropic",
  model = "claude-opus-5",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "low", "medium", "high", "xhigh", "max" } },
  },
}
rness.models.declare {
  provider = "anthropic",
  model = "claude-fable-5",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "low", "medium", "high", "xhigh", "max" } },
  },
}
rness.models.declare {
  provider = "anthropic",
  model = "claude-sonnet-5",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "low", "medium", "high", "xhigh", "max" } },
  },
}
rness.models.declare {
  provider = "anthropic",
  model = "claude-opus-4-6",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "low", "medium", "high", "max" } },
  },
}
rness.models.declare {
  provider = "anthropic",
  model = "claude-opus-4-8",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "low", "medium", "high", "xhigh", "max" } },
  },
}
rness.models.declare {
  provider = "anthropic",
  model = "claude-haiku-4-5-20251001",
  capabilities = {
    context_window = 200000,
    max_output_tokens = 64000,
    -- Manual thinking: budget must be >= 1024 and below request max_tokens.
    reasoning = { efforts = {}, budget_tokens = { min = 1024, max = 63999 } },
  },
}

-- ChatGPT: protocol = "chatgpt-responses", OAuth, provider name "chatgpt".
-- "openai-chatgpt" is the credential-store name in examples/init.lua.
rness.models.declare {
  provider = "chatgpt",
  model = "gpt-6-astra",
  capabilities = {
    context_window = 1050000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "low", "medium", "high", "xhigh", "max" } },
  },
}
rness.models.declare {
  provider = "chatgpt",
  model = "gpt-5.6-sol",
  capabilities = {
    context_window = 1050000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "none", "low", "medium", "high", "xhigh", "max" } },
  },
}
rness.models.declare {
  provider = "chatgpt",
  model = "gpt-5.6-terra",
  capabilities = {
    context_window = 1050000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "none", "low", "medium", "high", "xhigh", "max" } },
  },
}
rness.models.declare {
  provider = "chatgpt",
  model = "gpt-5.6-luna",
  capabilities = {
    context_window = 1050000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "none", "low", "medium", "high", "xhigh", "max" } },
  },
}
rness.models.declare {
  provider = "chatgpt",
  model = "gpt-5.5",
  capabilities = {
    context_window = 1050000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "none", "low", "medium", "high", "xhigh" } },
  },
}
rness.models.declare {
  provider = "chatgpt",
  model = "gpt-5.4",
  capabilities = {
    context_window = 1050000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "none", "low", "medium", "high", "xhigh" } },
  },
}
rness.models.declare {
  provider = "chatgpt",
  model = "gpt-5.4-mini",
  capabilities = {
    context_window = 400000,
    max_output_tokens = 128000,
    reasoning = { efforts = { "none", "low", "medium", "high", "xhigh" } },
  },
}
rness.models.declare {
  provider = "chatgpt",
  model = "gpt-5.3-codex-spark",
  capabilities = {
    context_window = 128000,
    max_output_tokens = 32000,
  },
}

-- OpenCode Go: provider-specific limits, not the underlying lab's limits.
-- Reasoning controls are omitted where the gateway's accepted values have not
-- been verified; omission does NOT mean that a model cannot reason.
-- Register "opencode-go" with protocol = "openai-chat", base_url =
-- "https://opencode.ai/zen/go/v1", auth = { env = "OPENCODE_GO_API_KEY" }.
rness.models.declare {
  provider = "opencode-go",
  model = "kimi-k2.7-code",
  capabilities = {
    context_window = 262144,
    max_output_tokens = 262144,
  },
}
rness.models.declare {
  provider = "opencode-go",
  model = "kimi-k2.6",
  capabilities = {
    context_window = 262144,
    max_output_tokens = 65536,
  },
}
rness.models.declare {
  provider = "opencode-go",
  model = "glm-5.2",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 131072,
  },
}
rness.models.declare {
  provider = "opencode-go",
  model = "glm-5.1",
  capabilities = {
    context_window = 202752,
    max_output_tokens = 32768,
  },
}
rness.models.declare {
  provider = "opencode-go",
  model = "deepseek-v4-pro",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 384000,
  },
}
rness.models.declare {
  provider = "opencode-go",
  model = "deepseek-v4-flash",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 384000,
  },
}
rness.models.declare {
  provider = "opencode-go",
  model = "mimo-v2.5",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 128000,
  },
}
rness.models.declare {
  provider = "opencode-go",
  model = "mimo-v2.5-pro",
  capabilities = {
    context_window = 1048576,
    max_output_tokens = 128000,
  },
}

-- Register "opencode-go-anthropic" with protocol = "anthropic" and the same
-- Go base_url/auth. These models use /messages, NOT /chat/completions.
rness.models.declare {
  provider = "opencode-go-anthropic",
  model = "minimax-m3",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 131072,
  },
}
rness.models.declare {
  provider = "opencode-go-anthropic",
  model = "minimax-m2.7",
  capabilities = {
    context_window = 204800,
    max_output_tokens = 131072,
  },
}
rness.models.declare {
  provider = "opencode-go-anthropic",
  model = "qwen3.7-max",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 65536,
  },
}
rness.models.declare {
  provider = "opencode-go-anthropic",
  model = "qwen3.7-plus",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 65536,
  },
}
rness.models.declare {
  provider = "opencode-go-anthropic",
  model = "qwen3.6-plus",
  capabilities = {
    context_window = 1000000,
    max_output_tokens = 65536,
  },
}
