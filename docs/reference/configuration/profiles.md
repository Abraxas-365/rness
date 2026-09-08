# Profile configuration reference

Profiles combine a model connection, an opaque model ID, and optional generation preferences. They do not contain role instructions or tool permissions.

```lua
rness.profiles.declare("coding", {
  provider = "anthropic",
  model = "claude-sonnet-5",
  options = {
    max_output_tokens = 8192,
  },
})
```

The connection must be configured, and the model must be available to your account. `coding` is an arbitrary user-defined name, not a built-in profile.

## Fields

| Field | Type | Required | Behavior |
| --- | --- | --- | --- |
| Name argument | string | Yes | Nonempty, unique startup profile name |
| `provider` | string | Yes | Nonempty connection name |
| `model` | string | Yes | Nonempty upstream model ID |
| `options` | table | No | Omitted means no explicit generation preferences |
| `options.reasoning` | tagged table | No | Reasoning effort or token budget |
| `options.max_output_tokens` | positive integer | No | Explicit output limit |
| `options.temperature` | finite number | No | Sampling preference |

Unknown declaration and option fields are rejected. Model-capability validation occurs when a profile is resolved; registering a profile is not a network check.

## Reasoning shapes

Named effort:

```lua
reasoning = { kind = "effort", effort = "high" }
```

Manual budget:

```lua
reasoning = { kind = "budget_tokens", tokens = 4096 }
```

These are alternatives, not two simultaneous settings. The service requires manual budgets of at least 1024 tokens. The Anthropic adapter supports manual-budget and adaptive-effort mappings. The OpenAI-compatible and ChatGPT Responses adapters support effort, not manual token budgets.

Acceptance of a named effort by configuration parsing does not guarantee that a particular model or gateway supports it. Do not infer enabled reasoning from whether a stream includes visible thinking text.

If output and thinking budgets are incompatible, the request fails rather than silently increasing an explicit output limit. Omitted generation fields retain adapter behavior, which may itself include protocol defaults.

## Defaults and resolution

```lua
rness.default_profile = "coding"
```

This supplies a fallback for new sessions under the conditions in [Configuration precedence](../configuration-precedence.md). It does not replace a resumed session's saved configuration automatically.

After startup, plugins can resolve a profile to a fresh configuration value:

```lua
local config = rness.profiles.resolve("coding")
```

Resolving alone does not apply it to a session. A profile resolution contains generation settings, not an agent snapshot. Preserve the current agent explicitly if using the generic session configuration setter to switch profiles.

## Declared capabilities

Capabilities are optional facts about a provider/model pair, not profile preferences. The current registry supports optional context and output limits, supported effort names, and a manual-budget range. Unknown capability data remains unknown; the repository does not promise a complete maintained model catalog.

Implementation: [registry and validation](../../../crates/rness-engine/src/config.rs), [request configuration types](../../../crates/rness-protocol/src/events.rs).
