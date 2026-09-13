# Agent configuration reference

An agent declaration defines a reusable role. All declared roles are eligible for principal selection; only roles explicitly enabled for delegation enter the named subagent roster.

```lua
rness.agents.declare("worker", {
  description = "Implements scoped changes and runs tests.",
  instructions = "Follow project conventions and report verification results.",
  subagent = true,
  -- profile = "coding",
  -- tools = {},
})
```

## Fields

| Field | Type | Required | Omitted behavior |
| --- | --- | --- | --- |
| Name argument | string | Yes | Must be nonempty after trimming and unique |
| `description` | string | Yes | Must be nonempty; shown in the delegation roster when enabled |
| `instructions` | string | Yes | Must be nonempty; appended to the base system instructions for turns |
| `profile` | string | No | Preserve principal generation settings or inherit the parent's settings |
| `tools` | list of strings | No | No additional role restriction |
| `subagent` | boolean | No | `false`: not selectable by name through delegation |
| `sandbox` | `read-only`, `workspace-write`, or `danger-full-access` | No | Inherits the global default for new sessions and preserves any tighter session/parent policy |

Unknown fields fail deserialization. Referenced profiles are resolved during startup validation. Declarations close after startup and require a restart to change.

An omitted `tools` field and `tools = {}` are different: the empty list allows no tools. A nonempty list uses exact engine-registered tool names; it is not a list of conceptual categories such as “filesystem.”

## Principal selection

```lua
rness.default_agent = "worker"
```

The default must name a declared agent. It applies to new sessions, not automatically to resumed sessions. Explicit CLI selection is available:

```sh
rness --agent worker --profile coding
```

The profile in this command must have been declared. An explicit CLI profile overrides the agent's profile while preserving the role snapshot. See [precedence](../configuration-precedence.md).

After engine mount:

```lua
local snapshot = rness.session.agent(session_id)
local selected = rness.session.agent(session_id, "worker")
```

The getter returns the serialized optional snapshot. The selector requires an idle session, validates the name and declared tool names against the current registry, resolves an optional profile, and appends effective request configuration if changed. Errors are raised as Lua errors. This API has no dedicated clear-role operation.

The snapshot contains `name`, `instructions`, and optional `tools`. It is not a live pointer to the declaration. Resuming can retain these values after the declaration changes or disappears; selecting the role again requires the current declaration.

## Delegation eligibility

`subagent = false` is enforced by the engine, not merely hidden in the model-facing schema. Unknown and disabled names fail before the runtime creates a child. This applies to one-shot and continuable delegation.

```json
{"provider":"spawn","agent":"worker","prompt":"Implement the assigned change."}
```

An unnamed child inherits generation settings without copying the parent's active role. Enabling a role for delegation does not prevent using it as the principal.

## Tool enforcement

Each turn restricts both advertised tool schemas and actual dispatch. A model request for a forbidden tool produces an error result rather than executing that tool.

Delegation captures the parent's effective tool names into a durable `tool_ceiling`. The child's role may restrict that set further. Changing the role or using the generic configuration setter cannot expand an existing ceiling. The ceiling is a snapshot: later parent changes do not retroactively rewrite it.

This is not process isolation. Plugins are trusted code, and an allowed shell tool can have broad effects. Approval policy is a separate control.

For Bash filesystem confinement and the global default that also applies without an agent role, see [sandbox configuration](sandbox.md).

## Current validation boundaries

CLI principal seeding and service-based selection do not yet share every validation path. In particular, CLI startup snapshots do not use the service selector's unknown-tool check. Do not rely on an invalid tool name always producing the same error at every entry point. The actual turn registry still restricts tools to names that exist.

The TUI completes declared roles for `/agent <name>`. The service recognizes that command in a send containing exactly one text part, so HTTP and TUI share selection and validation. It persists configuration without a user message or model turn; the current send response uses `log_only`, not a separate command response type. Missing, extra, or unknown names fail. There is no public `rness.agents.list()` query. The mounted `rness.subagents.roster()` query exposes delegable names and descriptions only.

Implementation: [declarations](../../../crates/rness-lua/src/api/config.rs), [service selection](../../../crates/rness-engine/src/service.rs), [turn enforcement](../../../crates/rness-engine/src/turn/mod.rs).
