# Subagent slash commands

The default flavor enables `plugins/agent-controls.lua`:

- `/agents` lists running descendants with their session IDs and parents.
- `/agents stop <id>` requests cancellation of that child's current turn.
- `/agents stop` stops the child when exactly one descendant is running. With
  several running children it lists them instead; with none it reports that.

Type `/agents stop ` and use the normal slash-command completion menu to select
an ID, just as with `/model`. Completions include only currently running
subagents belonging to this session. A stale or unrelated ID is rejected; IDs
must match exactly (no ambiguous prefix matching).

Both one-shot and continuable descendants are included, even under a one-shot
parent. These IDs are **session IDs**, not background job IDs. Stopping a child
requests cancellation; it does not wait for cleanup or cancel its parent or
other descendants. Continuable children remain available for later messages.

The command and completion work while the parent is running and do not enter its
user-input queue. Existing background-result delivery is unchanged. The former
`x`/`selection_stop` subagent-stop key action has been removed. Message selection,
expansion, copy, and editor shortcuts remain available. Old `selection_stop`
configuration is accepted but ignored for compatibility.

Existing installations are not overwritten by a flavor update. Copy
`flavors/default/plugins/agent-controls.lua` to `~/.rness/plugins/` and add this
entry to the existing `rness.plugins.setup` list, then restart with a new binary:

```lua
{ name = "agent-controls", file = "plugins/agent-controls.lua" }
```

## Lua extension API

`rness.subagents.list(session)` returns all delegated descendants, including
one-shot children, with `session`, `parent`, `depth`, and `running` fields.
`rness.subagents.children` and the model's `list_agents` tool retain their existing
continuable-only behavior. `rness.subagents.interrupt(caller, target)` still
validates durable ancestry before cancellation.

Commands opt into busy execution using `allow_busy = true`. The default is false.
Only use it for operations safe alongside the session writer: reading live state
or interrupting descendants, not parent-history mutation. Command/extension
reservations and idle-only mutation checks remain in force.
