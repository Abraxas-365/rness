# First configuration and session

**Outcome:** run a session using an explicitly configured local connection and a reusable model profile.

## Before you begin

Build or install rness using [Install from source](../guides/installation/from-source.md). This tutorial assumes an OpenAI-compatible Ollama endpoint is already running at `http://localhost:11434/v1`, with `qwen3:14b` available. Substitute an installed model ID if necessary. This tutorial does not install or start Ollama.

## 1. Create your startup configuration

Create `~/.rness/init.lua` in your editor. If it already exists, merge the following declarations rather than replacing the file:

```lua
rness.providers.register("ollama", {
  protocol = "openai-chat",
  base_url = "http://localhost:11434/v1",
  auth = false,
})

rness.profiles.declare("local-qwen", {
  provider = "ollama",
  model = "qwen3:14b",
})
```

`ollama` names a connection. `local-qwen` names a reusable profile. Neither is an automatic alias for a model built into rness.

`auth = false` means this connection does not send an Authorization header. Use it only with an endpoint intended to accept unauthenticated requests.

## 2. Run one prompt

```sh
rness --profile local-qwen --approval ask -p "Reply with one short sentence introducing yourself."
```

The command runs headlessly and prints the transcript. A successful response verifies the endpoint, model selection, and request path together. A connection error usually means the server is unavailable; a model error may mean the model ID is not installed.

`--approval ask` requests approval for sensitive tools. The CLI default is `allow`, not `ask`. Approval policy is not an operating-system sandbox.

## 3. Open the terminal interface

```sh
rness --profile local-qwen --approval ask
```

To reuse this profile without specifying it on each new session, add:

```lua
rness.default_profile = "local-qwen"
```

Restart rness after changing startup Lua. Production startup configuration does not currently support hot reload.

## 4. Find and resume a session

```sh
rness --list
rness --session SESSION_ID --approval ask
```

Replace `SESSION_ID` with an ID from the listing. Resuming restores durable request configuration; it still needs the referenced provider connection and its credentials to be available in the current environment.

## Next steps

- [Define an agent](first-agent.md).
- [Organize init.lua](../guides/configuration/init-lua.md).
- [Look up provider and authentication fields](../reference/configuration/providers.md).
- [Understand configuration precedence](../reference/configuration-precedence.md).
