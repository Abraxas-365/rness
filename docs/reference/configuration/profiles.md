# Profile configuration reference

Profiles combine a model connection, an opaque model ID, and optional generation preferences. They do not contain role instructions or tool permissions.

```lua
rness.profiles.declare("coding", {
  provider = "anthropic",
  model = "claude-sonnet-5",
  options = {
    max_output_tokens = 32000,
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
| `options.max_output_tokens` | positive integer | No | Explicit per-request output limit; it overrides the adapter fallback and cannot exceed the model's declared `max_output_tokens` |
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

If output and thinking budgets are incompatible, the request fails rather than silently increasing an explicit output limit. For Anthropic, omitted `options.max_output_tokens` uses the adapter's 32,000-token fallback; a profile should set an explicit value when a workload needs a smaller or larger budget. For ChatGPT Responses, an explicit profile value is sent as `max_output_tokens`; omitting it leaves the field out so ChatGPT selects its model default. The declared model `max_output_tokens` remains a ceiling, not a request default. Other adapters may omit the request field and let their upstream service select a default.

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

## Compaction profiles

Compaction can use a separate profile without changing the principal session's configuration:

```lua
rness.profiles.declare("compactor", {
  provider = "YOUR_CONFIGURED_PROVIDER",
  model = "YOUR_SUMMARY_MODEL",
  options = {
    reasoning = { kind = "effort", effort = "low" }, -- if supported by this model
  },
})

-- Add after your existing complete compaction policy declaration:
rness.compaction.default.summary_profile = "compactor"
```

`summary_profile` is optional and must name a declared profile. A `by_provider` profile resolves against the provider of the session being compacted (including child sessions). A fixed profile can explicitly choose another provider; that provider receives the context being summarized.

- Without a profile, compaction inherits the current session's model and generation settings.
- With a profile, it uses that profile's model and generation settings. Omitted profile options use adapter defaults, not the principal's settings. Agent instructions and permissions are not inherited into the profile.
- `summary_tokens` overrides the profile/session output limit when the adapter supports it. Otherwise no output limit is sent; notably the ChatGPT Responses adapter does not enforce this cap.
- The resulting settings are validated before sending. In particular, a manual thinking budget must be smaller than `summary_tokens`. Unknown profiles, missing provider variants, and unavailable routes fail explicitly; there is no fallback model.
- Summary requests advertise no tools and use the compaction policy's `system_prompt` and `prompt`.
- Automatic compaction, the default `/compact`, region APIs, and the legacy session/server compaction API share profile resolution. The legacy API retains its existing region/checkpoint behavior and, without a configured policy, its built-in prompts.
- Provider/model policy keys refer to the **principal session model**, not the summarizer. Set thresholds for that session's context; separately ensure the summary model can accept the selected context.

Profile and automatic policy changes require restarting rness. Resolving a profile does not verify remote account/model availability.

### Migration from `summary_selection`

The inline `summary_selection = { route = ..., model = ... }` field was removed. Declare a profile using `provider` and `model`, then set `summary_profile` to its name. The old field is rejected with a migration error, including in manual region policies; it is never silently ignored.

## Declared capabilities

Capabilities are optional facts about a provider/model pair, not profile preferences. The current registry supports optional context and output limits, supported effort names, and a manual-budget range. Unknown capability data remains unknown; the repository does not promise a complete maintained model catalog.

Implementation: [registry and validation](../../../crates/rness-engine/src/config.rs), [request configuration types](../../../crates/rness-protocol/src/events.rs).
