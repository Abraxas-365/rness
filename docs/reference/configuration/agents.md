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
| `instructions` | string | Yes | Must be nonempty; appended after the [base system prompt](#system-prompt) for turns |
| `profile` | string | No | Preserve principal generation settings or inherit the parent's settings |
| `tools` | list of strings | No | No additional role restriction |
| `subagent` | boolean | No | `false`: not selectable by name through delegation |
| `sandbox` | `read-only`, `workspace-write`, or `danger-full-access` | No | Inherits the global default for new sessions and preserves any tighter session/parent policy |

Unknown fields fail deserialization. Referenced profiles are resolved during startup validation. Declarations close after startup and require a restart to change.

An omitted `tools` field and `tools = {}` are different: the empty list allows no tools. A nonempty list uses exact engine-registered tool names; it is not a list of conceptual categories such as “filesystem.”

## System prompt

```lua
rness.system_prompt.base = "You are rness, a coding agent. Be concise."
rness.system_prompt.section({
  name = "tool:Bash", order = 1000, tools = { "Bash" },
  text = "Check the [exit code: N] marker on every Bash result.",
})
```

Each request's system prompt is assembled in this order, parts joined by blank lines:

1. `base`
2. the active role's `instructions`
3. applicable sections, by ascending `order`, then by `name`
4. the structured-output instruction (workflow members with a schema), the file-reference guidance, plan-mode guidance and the task snapshot

rness has no built-in base: when `base` is unset or `""`, requests start directly with the role's instructions, or with the first applicable section. The base is trimmed. `rness.system_prompt` accepts only `base` and `section`, and anything else fails startup.

### Sections

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `name` | string | Yes | Unique, nonempty. Duplicates fail startup. |
| `text` | string | Yes | Nonempty after trimming, at most 4096 bytes |
| `order` | integer | No (0) | Ascending position among sections. Sections always follow the role instructions, whatever the order. |
| `tools` | list of strings | No (always) | Send the section only on steps where at least one of these tools is usable |

A tool counts as usable on a step when it is registered, allowed by the role and the parent's tool ceiling, and either advertised to the model or activated through `ToolSearch`. In `ptc` and `both` exposure modes, tools callable from `run_code` also count; that excludes `workflow`, which programs cannot call. Guidance therefore follows the tool: a read-only `scout` role does not receive Write/Edit guidance, the workflow section disappears when `rness.workflow` is unset, and a deferred tool's section appears once `ToolSearch` activates it. Tool names are exact, and an unknown name does not fail startup: that section is simply never sent.

Sections are static text, checked against the tools on each step. Showing or hiding a section changes the system prompt, which invalidates the provider's prompt cache from that point on. For a walkthrough with examples, see the [system prompt guide](../../guides/system-prompt.md).

#### Tool-owned sections

A plugin tool can carry its own section:

```lua
rness.tool.register {
  name = "session_search", description = "…", schema = { … }, run = …,
  prompt = { order = 2300, text = "Use session_search to find prior work; follow hits with session_event_read." },
  -- or: prompt = "text"   (order 4000, after the built-in tool sections)
}
```

The section is named `tool:<tool name>` and is tied to that tool alone, so it follows the same usability rule. Because it belongs to the tool, it ships with the plugin, is replaced on hot reload, and disappears when the plugin is disabled or unloaded. `prompt` accepts only `text` (required, nonempty after trimming, at most 4096 bytes) and `order` (integer); anything else fails the plugin load. For a family of tools, put the guidance on the entry-point tool, as [`plugins/session-search.lua`](../../../flavors/default/plugins/session-search.lua) does on `session_search`.

A `rness.system_prompt.section` with the same name (for example `tool:session_search`) replaces the tool's section, so you can reword a plugin's guidance from init.lua without editing the plugin.

The default flavor declares its base and sections for built-in tools in [`lua/system_prompt.lua`](../../../flavors/default/lua/system_prompt.lua), using dsh's order numbers (Bash 1000, … workflow 2600). Sections carry only guidance that tool descriptions do not: when to prefer one tool over another and how tools combine (Bash, Edit, background jobs, terminal, web search, workflow). Keep the base short and role-neutral, because it also precedes every subagent role's instructions. Config declarations close after startup, so restart after changing them.

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

### Restrict delegation to the roster

Generic (unnamed) children are **disabled by default**. Omission is equivalent to this setting in `~/.rness/init.lua`:

```lua
rness.agents.allow_generic = false
```

This global, startup-only boolean is separate from individual agent declarations; restart to apply changes. Only roles declared with `subagent = true` may be used while generic children are disabled. The model-facing tool requires `agent` and tells the model not to delegate if no configured role fits. Unknown names cannot create new roles. If the delegable roster is empty, the tool explicitly reports delegation as unavailable and all requests are rejected; it remains registered so existing role tool allowlists stay valid.

The runtime rejects unnamed requests before child creation for both `spawn` and `fork`, including foreground, background one-shot, continuable, and Lua delegation. The policy applies to new delegation from all sessions and descendants, including resumed sessions; it does not terminate existing children or prevent messaging them. To opt in to generic delegation, set `rness.agents.allow_generic = true`. The default flavor exposes `rness.agents.allow_generic = false` at the top of `lua/agents.lua` so users can easily change it.

## Tool enforcement

Each turn restricts both advertised tool schemas and actual dispatch. A model request for a forbidden tool produces an error result rather than executing that tool.

Delegation captures the parent's effective tool names into a durable `tool_ceiling`. The child's role may restrict that set further. Changing the role or using the generic configuration setter cannot expand an existing ceiling. The ceiling is a snapshot: later parent changes do not retroactively rewrite it.

This is not process isolation. Plugins are trusted code, and an allowed shell tool can have broad effects. Approval policy is a separate control.

For Bash filesystem confinement and the global default that also applies without an agent role, see [sandbox configuration](sandbox.md).

## Current validation boundaries

CLI principal seeding and service-based selection do not yet share every validation path. In particular, CLI startup snapshots do not use the service selector's unknown-tool check. Do not rely on an invalid tool name always producing the same error at every entry point. The actual turn registry still restricts tools to names that exist.

The TUI completes declared roles for `/agent <name>`. The service recognizes that command in a send containing exactly one text part, so HTTP and TUI share selection and validation. It persists configuration without a user message or model turn; the current send response uses `log_only`, not a separate command response type. Missing, extra, or unknown names fail. There is no public `rness.agents.list()` query. The mounted `rness.subagents.roster()` query exposes delegable names and descriptions only.

## Monitor presentation

`rness.ui.messagebox.agents` configures the read-only `/agents` drawer, not role
declarations. It uses the existing messagebox startup configuration; restart
with the updated binary and theme to apply edits. See the
[customization example](../../guides/subagent-commands.md#customizing-the-read-only-drawer).

### Keys

Each `keys` field accepts one chord string, a nonempty array of chord strings,
or `false`. Overrides replace all defaults for that action. Chords use the
shared key language (`ctrl+u`, `shift+tab`, `<F6>`, etc.).

| Field | Default | Context |
| --- | --- | --- |
| `back` | `esc` | Detail → list → close |
| `previous_agent` / `next_agent` | `shift+tab` / `tab` | List and detail |
| `list_up` / `list_down` | `{ "up", "k" }` / `{ "down", "j" }` | List only |
| `open_detail` | `enter` | List only |
| `metadata_up` / `metadata_down` | `alt+pageup` / `alt+pagedown` | Detail metadata |
| `page_up` / `page_down` | `pageup` / `pagedown` | Detail transcript |
| `follow` | `end` | Resume following latest output |
| `scroll_up` / `scroll_down` | `up` / `down` | Detail, outside message selection |
| `toggle_tool` | `{ "enter", "ctrl+o" }` | Detail, outside message selection |

Dispatch priority is back, previous/next agent, context-specific list actions,
metadata, transcript paging/follow, line scrolling/tool expansion, then inherited
Chat keys. Avoid overlapping bindings in the same context. Message selection,
copy, thinking, and tool navigation use the main messagebox keys. The monitor's
`toggle_tool` replaces Chat's tool-toggle shortcut outside message selection;
selection's own expansion key is unchanged. Footer and binding help derive from
the configured monitor keys; narrow footers clip to available width.

Keep a usable `back` key. No fixed Escape fallback is retained after rebinding
or disabling it. Read-only action filtering, descendant authorization, and
approval/question priority cannot be changed by these options.

### Layout, text, and styles

| `layout` field | Default | Range / behavior |
| --- | --- | --- |
| `list_rows` | All agents | 1–1000; maximum requested visible rows, plus border/footer; selection scrolls into view |
| `metadata_rows` | 3 | 0–1000; maximum task/identity rows; zero hides metadata; also limited to one third of available detail body |
| `page_lines` | 10 | 1–1000; transcript page step |
| `wheel_lines` | 3 | 1–1000; detail wheel step; list wheel still moves one agent |
| `metadata_scroll_lines` | 3 | 1–1000; metadata key step |

`text` supports `title` (`"Agents"`), `empty` (`"No agents published yet."`),
`waiting` (`"Waiting for an authorized agent entry (or agent not found)."`),
`incoming_label` (`"Incoming message"`), `following` (`"Following"`),
`paused` (`"Paused"`), and `paused_new` (`"Paused + new"`). Values are strings
of at most 256 bytes without control characters; empty strings are allowed.
The title retains the agent count and read-only indicator.

`styles` supports `frame`, `border`, `heading` (frame title), and `hint`, with
fallback theme roles `overlay`, `overlay_border`, `heading`, and `dim`.
Each accepts the usual messagebox style name or inline style table.
Other message and status colors continue to inherit the shared theme.

This is focused customization, not an arbitrary Lua drawer renderer: responsive
column widths/order, status mapping, metadata identity labels, rounded border,
and the read-only safety boundary remain built in.

Implementation: [declarations](../../../crates/rness-lua/src/api/config.rs), [service selection](../../../crates/rness-engine/src/service.rs), [turn enforcement](../../../crates/rness-engine/src/turn/mod.rs).
