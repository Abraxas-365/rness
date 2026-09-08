# Provider configuration reference

A provider declaration names a connection: wire protocol, endpoint, and authentication. It does not select a model.

```lua
rness.providers.register("router", {
  protocol = "openai-chat",
  base_url = "https://openrouter.ai/api/v1",
  auth = { env = "OPENROUTER_API_KEY" },
})
```

## Fields

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| Name argument | string | Yes | Nonempty connection name; cannot contain `/` |
| `protocol` | string | Yes | `openai-chat`, `anthropic`, or `chatgpt-responses` |
| `base_url` | string | Yes | HTTP(S) endpoint base appropriate to the adapter |
| `auth` | boolean or table | Yes | Explicit authentication strategy below |

Duplicate startup provider names fail. Unknown top-level declaration fields fail. User declarations can replace built-in connection names when the CLI assembles its route table.

## Authentication

Choose one strategy:

| Declaration | Resolution |
| --- | --- |
| `auth = false` | No authentication; supported for `openai-chat` only |
| `auth = { env = "VARIABLE" }` | Read exactly that nonempty environment variable as an API key |
| `auth = { credential = "entry" }` | Read the API key from exactly that credential-store entry |
| `auth = { oauth = "entry" }` | Use stored OAuth tokens and the adapter's refresh behavior |

Explicit environment and stored-key strategies do not silently fall back to each other. `auth = true` is invalid. OAuth is supported by the Anthropic and ChatGPT Responses adapters, not the generic OpenAI chat adapter. ChatGPT Responses requires OAuth rather than an API key.

Example stored-key management:

```sh
rness auth set-key --provider router
rness auth status
```

With no key positional argument, `set-key` reads standard input. Avoid placing keys directly in shell command arguments, where history or process inspection may expose them. Browser login is available through `rness auth login --provider anthropic` and `rness auth login --provider openai-chatgpt`.

Custom OAuth store references select an entry; they do not define an arbitrary OAuth issuer or login implementation.

## Endpoint validation

URLs must parse with a host and use HTTP or HTTPS. Usernames, passwords, query strings, and fragments are rejected. Include required adapter prefixes such as `/v1` for an OpenAI-compatible endpoint. A syntactically valid URL does not prove protocol compatibility.

Prefer HTTPS for remote endpoints. Plain HTTP is useful for local development but does not protect transmitted credentials or prompts.

## Model IDs

A profile stores the connection and model ID separately:

```lua
rness.profiles.declare("router-sonnet", {
  provider = "router",
  model = "anthropic/claude-sonnet-4.5",
})
```

The model ID is opaque to selection parsing. This example must be replaced if your endpoint offers a different ID. In combined CLI syntax, only the first slash separates the connection:

```sh
rness --model router/anthropic/claude-sonnet-4.5
rness --provider router --model anthropic/claude-sonnet-4.5
```

These selections identify the same connection/model pair. The `router/` prefix is rness's connection namespace, not part of the upstream model ID.

## Legacy CLI routes

`--route 'name=url[,credential|,none]'` declares an OpenAI-compatible connection for that invocation. A CLI route overrides the same-name startup connection and removes its explicit startup auth strategy. Prefer `init.lua` declarations for reusable configuration with unambiguous authentication.

Implementation: [startup declaration parser](../../../crates/rness-lua/src/api/config.rs), [CLI composition](../../../crates/rness-cli/src/main.rs), [provider routes](../../../crates/rness-providers/src/routes.rs).
