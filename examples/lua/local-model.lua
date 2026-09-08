-- Copy to ~/.rness/lua/local-model.lua; require("local-model") in init.lua.
-- Prerequisite: this endpoint is running and this model is installed.
rness.providers.register("local", {
  protocol = "openai-chat",
  base_url = "http://localhost:11434/v1",
  auth = false,
})
rness.profiles.declare("local-coding", {
  provider = "local",
  model = "qwen3:14b",
  options = { max_output_tokens = 4096 },
})
-- Optional: rness.default_profile = "local-coding"
