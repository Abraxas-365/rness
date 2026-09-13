# Subagent slash commands

The default flavor enables `plugins/agent-controls.lua`:

- `/agents` opens the agent monitor; `/agents <id>` focuses one descendant.
- `/agents <id> steer <message>` sends a separate user-origin steering message
  to a continuable descendant; one-shot targets are rejected by the runtime.
  Missing text returns usage, not a composer.
- `/agents <id> stop` asks for confirmation to cancel that child's current turn.
- `/agents <full-id> stop confirm` performs the cancellation. The notice supplies
  this exact command and explains that queued messages and descendants remain.
- `/agents stop <id>` remains a compatibility spelling for requesting confirmation.
- `/agents stop` selects the child when exactly one descendant is running. With
  several running children it lists them instead; with none it reports that.

Use the normal slash-command completion menu to select an alias such as `a1`.
Each agent suggestion shows its role and a short preview of its assigned task;
the preview is display-only and is never inserted into the command. Full session
IDs remain accepted when entered manually, but are not duplicate suggestions.
Aliases cover all descendants including idle children,
are append-only per root, and keep tombstones for removed sessions. Running-state
changes, deletion, and imports do not retarget an alias during this runtime.
Aliases reset after restart: use a full ID for durable identity. Unrelated IDs and ambiguous
prefixes are rejected. Questions does not expose a generic asynchronous command
prompt API, so confirmation uses an explicit command instead of a dialog.

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

## Reading an agent's execution

`/agents` opens the live list; select an agent with ↑/↓ and press Enter, or use
`/agents a1` directly. On a selected delegation or `send_message` card in the main
conversation, press `i` to inspect its associated child.

The drawer is **read-only**: no composer, submission, steering, stop, or editor
controls. It uses the main conversation's Chat renderer and theme, including
Markdown, syntax-highlighted code, diffs, and individually expandable tool cards.
Incoming messages retain their text and use a neutral label rather than claiming
that every assignment came from you.

- Enter or Ctrl+O expands the focused tool; Alt+↑/↓ moves between tools.
- PgUp/PgDn or the mouse wheel scrolls the child, not the hidden main conversation.
- Scrolling pauses auto-follow; End follows the latest output again.
- Tab/Shift+Tab switches agents; Alt+M selects messages for copying.
- Alt+PgUp/PgDn scrolls long task/identity metadata.
- Esc returns from detail to the list, then closes the drawer.

Idle and finished children remain inspectable. Main-session identity, draft and
scroll position are unchanged. Only the separate steering command sends input;
its coordinator notice is injected without waking an idle main agent.

Existing installations need both the updated command plugin and the agent/message
renderers in `flavors/default/lua/theme.lua` to get the new default cards. Merge
the theme changes into custom themes rather than overwriting local customization.

## Lua extension API

`rness.subagents.list(session)` returns all delegated descendants, including
one-shot children, with `alias`, `session`, `parent`, `depth`, and `running` fields.
Pass `true` as a second argument for cached `name` and assigned `task` preview
fields used by completion hints. The default call avoids task-history reads.
`rness.subagents.children` and the model's `list_agents` tool retain their existing
continuable-only behavior. `rness.subagents.interrupt(caller, target)` still
validates durable ancestry before cancellation. `rness.subagents.steer_user`
is a separate trusted user-control API with the same descendant scope: it records
user provenance durably and asynchronously notifies the invoking conversation,
without pretending the principal agent authored the message. Model-facing
`send_message` keeps its existing adjacency restrictions.

Monitor commands return `{ data = { action = "agents:open", session = ctx.session,
agent = full_child_id } }`, omitting `agent` for the overview. The CLI explicitly
routes this allowlisted data action to the monitor. See the
[Lua subagents reference](../reference/lua/subagents.md) for delivery semantics.

Commands opt into busy execution using `allow_busy = true`. The default is false.
Only use it for operations safe alongside the session writer: reading live state
or interrupting descendants, not parent-history mutation. Command/extension
reservations and idle-only mutation checks remain in force.
