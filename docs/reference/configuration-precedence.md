# Configuration precedence

This page describes current CLI composition and service-based role selection. It is not a generic deep-merge system. In particular, applying a profile replaces generation options rather than merging individual profile fields with previous settings.

## CLI startup sequence

1. Start with an empty request configuration for a new session, or replay saved configuration for `--session`.
2. Select `--agent` when supplied. Otherwise use `default_agent` for a new session only.
3. If that agent declares a profile, resolve it and apply its generation settings. Record the agent snapshot.
4. Apply explicit `--profile`, preserving the selected snapshot. Without it, `default_profile` applies only to a new session with no explicit model and no chosen agent profile.
5. Apply explicit model selection. If the provider/model pair changes, reset prior generation options while preserving the role snapshot.
6. Apply explicit reasoning, output-token, and temperature flags.
7. Preserve any inherited tool ceiling from the resumed configuration.
8. Validate declared model capabilities and construct the selected provider.

## Common combinations

| Inputs | Result |
| --- | --- |
| New session, `default_profile` | Uses that profile |
| New session, `default_agent` with a profile | Agent profile wins over `default_profile` |
| New session, agent without a profile | Needs an explicit model/profile or `default_profile` |
| `--agent` plus `--profile` | CLI profile wins; agent instructions remain |
| `--agent` plus changed `--model` | Explicit model wins and prior generation options reset |
| `--profile` plus `--model` | Rejected by CLI argument parsing |
| `--provider` without `--model` | Rejected by CLI argument parsing |
| Resume with no overrides | Restores saved request configuration |
| Resume after changing defaults | Defaults do not replace the saved selection or role |
| Resume with explicit `--agent` | Selects the current declaration; its profile applies if present |

If an explicit model equals the saved or profile-selected pair, the CLI does not perform the changed-model reset. Explicit generation flags still override their respective fields.

## Mid-session agent selection

`rness.session.agent(id, name)` requires idle:

- With a profile: replace generation settings using the profile.
- Without a profile: preserve current generation settings.
- Replace the role snapshot.
- Preserve the tool ceiling.
- Append request configuration only if the effective value changed.

Selecting the same role after editing its declaration requires restarting first because the startup registry is frozen.

## Delegation

A child begins with the parent's generation settings. A requested agent profile replaces them. The child gets a newly captured effective tool ceiling; the role can narrow but cannot expand it. Omitted `agent` is rejected by default. When startup configuration sets `rness.agents.allow_generic = true`, omission means no role snapshot, even when the parent has a role. This global startup policy applies to new delegation from resumed sessions too; it is not a per-session request option.

## Persistence versus connections

Request configuration stores selection and effective role data, not authentication secrets. Restoring a session does not recreate removed connection declarations or unavailable credentials. Profiles are startup conveniences; persisted resolved settings are not automatically recomputed from edited profiles.

Implementation: [CLI](../../crates/rness-cli/src/main.rs), [service](../../crates/rness-engine/src/service.rs).
