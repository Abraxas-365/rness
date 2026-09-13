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

Pass the role name in `spec.agent`. The first argument (`"spawn"` or `"fork"`) selects how the child starts, not its role: `spawn` starts a fresh conversation, while `fork` inherits completed parent history. Both support named roles, as does `start_continuable`.

First declare the role in startup configuration (`~/.rness/init.lua` or a required module):

```lua
rness.agents.allow_generic = false -- default; named delegation still works
rness.agents.declare("worker", {
  description = "Implements scoped changes.",
  instructions = "Implement the assigned task and verify it with tests.",
  subagent = true,
})
```

After engine mount, a plugin can delegate using that role:

```lua
local result = rness.subagents.start("spawn", {
  parent = session_id,
  agent = "worker",
  prompt = "Implement the agreed change and run the relevant tests.",
})
```

Unknown names and roles without `subagent = true` are rejected; passing a name does not create a new role. The default flavor already enables `scout` and `reviewer`; its `worker` declaration is commented out and must be enabled before using the example above. If no eligible role fits, do not delegate.

| Argument | Type | Required |
| --- | --- | --- |
| `provider` | string | Yes |
| `spec.parent` | session ID string | Yes |
| `spec.prompt` | string | Yes |
| `spec.agent` | string | Unless generic children are enabled |

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

## `steer_user(caller, target, text)`

Trusted user-control entry point, **not** the model-facing `send_message` tool. `caller` is the invoking conversation; `target` must be its strict delegated descendant, verified by walking durable ancestry. Self, ancestors, unrelated sessions, and whitespace-only messages are rejected. One-shot targets are rejected even through the runtime API; continuable descendants are supported, including beneath a one-shot ancestor. A running target receives steering at its next step boundary; an idle target starts a turn.

The target receives a durable plain `UserMessage` (`intent = steer` while running, normalized to `followup` when idle; no injected source) beginning `User steering from conversation <caller>:`. This deliberately differs from the `Agent <sender> sent a message:` frame used by agent messaging. The invoking conversation receives a separate `User intervened in subagent <target> with steering:` notice including the text. Notification uses a reservation-aware injection path asynchronously, so it cannot deadlock the command reservation; it never wakes an idle caller. Acceptance is not a reply or a guarantee the model has consumed the message. Notification failures are logged.

The API relies on trusted Lua supplying the actual invoking conversation, just like the existing interrupt API. It does not broaden model-facing messaging authorization or introduce a new wire-format source variant.

## `list(root, details?)`

Returns all delegated descendants, including idle and one-shot children, with `session`, `parent`, `depth`, `running`, and `alias`. Aliases `a1`, `a2`, … are assigned per root in full-ID order on first observation; later discoveries append regardless of their IDs. Removed sessions leave tombstones, so no existing alias is reassigned during the runtime's lifetime. Clients must use `child.alias`, **not** enumerate a filtered list. Aliases reset on process restart; full IDs remain the durable identity across restarts.

Pass `true` as the optional second argument to include `name` (the current role,
or `subagent` for an unnamed child) and `task` (a bounded preview of its original
assigned prompt). The task excludes inherited fork history and injected
instructions. These details are cached and updated from the child's own log;
omitting the flag avoids those reads, as used by the statusline.

The default completion menu uses aliases and shows `name · task` as a display-only
hint. Selecting a suggestion inserts only the command, never the hint. Full IDs
remain accepted as input and are used in durable stop-confirmation commands.

## Default `/agents` command

- `/agents`: open the agent monitor.
- `/agents <id>`: open the monitor focused on that descendant.
- `/agents <id> steer <message>`: send user steering separately; missing text returns usage and never opens a composer.
- `/agents <id> stop`: request confirmation to stop only that child's current turn.
- `/agents <id> stop confirm`: perform the interrupt. The confirmation notice uses the full ID. Queued messages are not cleared and descendants keep running; idle targets are accepted no-ops.
- `/agents stop <id>` remains supported as a confirmation request; `/agents stop` selects the sole running descendant for confirmation, otherwise reports the running list.

IDs are exact full session IDs or `a1`, `a2`, … aliases. Prefixes, unrelated sessions, and the current session itself are rejected. Commands remain available while the principal runs. Questions currently has no generic asynchronous command prompt API, so confirmation uses an explicit command rather than a dialog.

Monitor commands use the existing `CommandResult.data` envelope, with no new command schema:

```lua
return { data = {
  action = "agents:open",
  session = ctx.session,
  agent = full_child_id, -- omitted for /agents
} }
```

The host must explicitly route this allowlisted data action; it is not a generic action executor. Stop and steering return ordinary status messages, not monitor actions.

## `interrupt(caller, target)`

Interrupts a child according to runtime ancestry checks. It is not a general-purpose permission to interrupt unrelated sessions. An authorized interrupt of an idle target is a no-op.

## `children(root, scope)`

Returns child records containing `session`, `parent`, `depth`, and `running`. Discovery lists continuable children, not one-shot runs. Omit `scope` for direct children; use `"descendants"` for recursive discovery.

## `delegation(id)`

Returns lineage metadata with `parent`, `depth`, and `mode`, or no delegation value for an ordinary root session. Lua mode strings here are `one_shot` and `continuable`; the model tool's background-mode spelling is `one-shot`.

## Configuration and authority

Generic children are disabled by default. Both `start` and `start_continuable` reject omitted `agent` unless startup configuration explicitly sets `rness.agents.allow_generic = true`. With the default policy, select a declared role enabled with `subagent = true`, or do not delegate.

A named role is applied before the child's first prompt. Its optional profile replaces generation settings; without a profile those settings are inherited. An unnamed child inherits generation settings but no active role. All children receive a durable tool ceiling from the parent.

See [agent configuration](../configuration/agents.md) and [the subagent tool](../tools/subagent.md).

Implementation: [Lua bridge](../../../crates/rness-lua/src/api/subagents.rs), [delegation runtime](../../../crates/rness-engine/src/subagent.rs).
