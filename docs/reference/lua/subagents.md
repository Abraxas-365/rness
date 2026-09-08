# Lua subagents reference

`rness.subagents` is installed after engine mount. It exposes the same delegation runtime used by the model-facing tool. Calling it at the top level of startup `init.lua` is too early.

## `providers()`

```lua
local providers = rness.subagents.providers()
```

Returns sorted child-creation provider names. Built-in providers are `fork` and `spawn`. These are not LLM connection names.

## `roster()`

```lua
local roster = rness.subagents.roster()
```

Returns a mapping from enabled agent name to description. Principal-only definitions are omitted. This is distinct from listing running children.

## `start(provider, spec)`

```lua
local result = rness.subagents.start("spawn", {
  parent = session_id,
  agent = "worker",
  prompt = "Review the specified function and report edge cases.",
})
```

| Argument | Type | Required |
| --- | --- | --- |
| `provider` | string | Yes |
| `spec.parent` | session ID string | Yes |
| `spec.prompt` | string | Yes |
| `spec.agent` | string | No |

Returns a table with `session`, `stop`, and `output`. `stop` is `completed`, `aborted`, or `error`. `output` is the child's last nonempty assistant text after the activation boundary; inherited fork output is not reused as the new result.

This call blocks the Lua VM actor until the child settles. It does not block the whole engine, but other callbacks on that VM cannot run concurrently. Avoid it for work that needs responsive Lua callbacks or depends on callbacks on the same VM to finish.

Unknown provider, excessive delegation depth, invalid role, and service errors raise Lua errors. A settled child with `stop = "error"` is a returned run result, not necessarily an exception.

## `start_continuable(provider, spec)`

Accepts the same specification and returns the child's session ID after submitting its first prompt, without waiting for completion:

```lua
local child = rness.subagents.start_continuable("spawn", {
  parent = session_id,
  agent = "worker",
  prompt = "Inspect the task and report findings.",
})
```

Continuable children remain durable sessions that can accept later messages. Their settled turns generate notices in the parent. A child turn and its parent's processing should not be assumed to finish in the same callback.

## `send_message(sender, target, text)`

Sends a message across an authorized parent/child edge. A parent can message a direct continuable child; a child can message its direct parent. Arbitrary session-to-session messaging is rejected by the runtime.

A running target receives steering; an idle target may start a turn. The call reports acceptance, not a reply.

## `interrupt(caller, target)`

Interrupts a child according to runtime ancestry checks. It is not a general-purpose permission to interrupt unrelated sessions. An authorized interrupt of an idle target is a no-op.

## `children(root, scope)`

Returns child records containing `session`, `parent`, `depth`, and `running`. Discovery lists continuable children, not one-shot runs. Omit `scope` for direct children; use `"descendants"` for recursive discovery.

## `delegation(id)`

Returns lineage metadata with `parent`, `depth`, and `mode`, or no delegation value for an ordinary root session. Lua mode strings here are `one_shot` and `continuable`; the model tool's background-mode spelling is `one-shot`.

## Configuration and authority

A named role is applied before the child's first prompt. Its optional profile replaces generation settings; without a profile those settings are inherited. An unnamed child inherits generation settings but no active role. All children receive a durable tool ceiling from the parent.

See [agent configuration](../configuration/agents.md) and [the subagent tool](../tools/subagent.md).

Implementation: [Lua bridge](../../../crates/rness-lua/src/api/subagents.rs), [delegation runtime](../../../crates/rness-engine/src/subagent.rs).
