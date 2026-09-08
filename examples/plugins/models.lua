-- models.lua — YOUR model capability declarations.
--
-- Copyable example (zero magic): cp to ~/.rness/plugins/ and edit to
-- the models YOU use. rness ships ZERO model data — an undeclared
-- model returns nil from rness.models.get and consumers fall back to
-- their own literals (dsh: contextWindow is human-declared adapter
-- config; the pi-ai catalog is just someone else's declaration).
--
-- These numbers are YOURS to keep current. If they drift, nothing
-- breaks silently: the provider's server is the source of truth and
-- rejects invalid requests fail-loud (visible as attempts in the log).
--
-- Consumers in the other examples:
--   spinner.lua      context budget line ("46.3k/160.0k tok")
--   autocompact.lua  compact threshold from context_window
--
-- Fields (all optional, any table shape you want — rness.models is a
-- dumb registry; only declarer and consumers agree on meaning):
--   context_window  input token capacity
--   max_output      output cap per request
--   reasoning       valid --reasoning spellings for this model

rness.models.declare("anthropic/claude-sonnet-5", {
  context_window = 200000,
  max_output = 64000,
  -- 4.6+ adaptive thinking: named efforts.
  reasoning = { "low", "medium", "high" },
})

rness.models.declare("anthropic/claude-sonnet-4-5", {
  context_window = 200000,
  max_output = 64000,
  -- <= 4.5 manual thinking: token budgets (any number >= 1024).
  reasoning = { "1024", "8192", "32000" },
})

rness.models.declare("openai-chatgpt/gpt-5.5", {
  context_window = 400000,
  max_output = 128000,
  reasoning = { "minimal", "low", "medium", "high" },
})

rness.models.declare("deepseek/deepseek-chat", {
  context_window = 131072,
  max_output = 8192,
})
