# Known limitations

This page explains where you may run into problems, what to expect, and what you can do about them. It describes the current source version; your installed version may differ. It is not a roadmap.

## Configuration and plugin updates

- **Most configuration changes need a restart.** This includes providers, profiles, agents, and other declarations in `init.lua`. Plugins loaded later cannot add these startup settings.
- **Not every plugin can reload automatically.** Watched files and linked packages support reload; managed packages and inline plugin code do not support watching. See [plugin loading and lifecycle](../guides/plugins/loading-and-lifecycle.md).
- **Reload is not a full reset.** It replaces plugin registrations, but it does not undo files written, network requests, or other side effects. Lua modules loaded with `require` remain cached. If a plugin behaves unexpectedly after reload, restart rness.
- **Updating the binary does not update your copied plugins.** If you keep local copies of the default plugins, check whether a feature also requires changes to those files. For example, older compaction commands may still block the Lua statusline while they run.

## Terminal display and performance

- **Resizing a long conversation can still pause the display.** Changing the terminal width makes rness lay out the transcript again. Code previews now do less work, but expanded code and diffs still cost more to render. Collapsing tool output can help; it does not eliminate the cost of resizing a long history.
- **The display cache setting is not a total memory limit.** `cache_bytes` limits cached display rows, not the conversation itself or all Markdown, tool-card, and temporary rendering data.
- **Expanded tool output and thinking are not saved preferences.** Expansion state belongs to the current UI session; do not expect it to survive a restart. Expanding or collapsing content does not change what the model receives.
- **Custom statuslines may show incomplete or outdated information.** Some older model-registry examples are not session-aware. Treat displayed context size as an estimate, and do not assume every custom statusline shows the active session's selected model.
- **A delayed plugin action may be discarded if you keep typing or change focus.** This prevents an old result from changing the wrong draft or view. Retry the action if it did not apply. Help and keybinding listings also cannot describe every custom handler or internal control.

## Compaction and context limits

Compaction summarizes older conversation content to make room for new messages. It does not guarantee that every conversation will fit a model's context window.

### Setup and estimates

- **Automatic compaction is opt-in for each provider/model.** Configure budgets through `rness.compaction`; no route is enabled automatically. See [the example configuration](../../examples/init.lua). Do not enable the legacy `autocompact.lua` plugin for the same route.
- **Token counts are estimates, not exact provider counts.** Text, message overhead, images, and reserved output space all affect the estimate. If requests still exceed the context limit, leave more headroom in your configured budget. Image estimates are not billing estimates.
- **Context-limit recovery can still fail.** rness recognizes common provider errors, but gateways may report them differently. Retries are limited, and recovery must actually reduce the context before proceeding.

### Summaries and manual compaction

- **A summary may not make the conversation smaller.** Empty or non-shrinking summaries are not applied. A provider failure can also leave history unchanged; an unchanged result does not necessarily mean the provider completed successfully.
- **Compaction keeps the original events in the session log.** It reduces the context sent to the model, not the size of the stored log, and is not a way to erase sensitive content. It preserves the newest message and keeps tool calls paired with their results.
- **Summarization makes a model request and may incur a charge.** It uses the active session model unless you configure a [`summary_profile`](../reference/configuration/profiles.md#compaction-profiles). An unavailable summary profile/provider/model causes an error rather than silently falling back.
- **Manual compaction requires an idle session.** The default `/compact` command keeps Lua status updates responsive, but the session remains occupied until the command finishes. Other commands or prompts for that session must wait.
- **Region selection is an optional example, not a built-in workflow.** The [commands plugin example](../../examples/plugins/commands.lua) previews up to 100 messages. After reviewing a range in the overlay, close it and run `/compact-region confirm`; reviewing alone does not start compaction. If the conversation changed after selection, refresh the selection and try again.

### Logs and interrupted requests

Compaction logs can contain conversation text, tool output, request bodies, and inline images. Headers and credentials are excluded from the recorded request bodies, but sensitive information may still appear in the conversation itself. **Review and redact logs before sharing them.**

If compaction is interrupted, rness records the interruption on resume rather than automatically repeating the request. Usage details or response chunks may be missing. Recovery has not been tested against every provider or disk-failure scenario; writing the completion record and the updated context is not one indivisible operation.

## Agents, plugins, and security

- **A tool allowlist is not full isolation.** It restricts which tools an agent can call, not every effect an allowed tool can have. Approval settings are a separate control.
- **Only load plugins you trust.** Lua plugins and native extensions can access resources outside the tool allowlist. Plugin search paths and callback limits do not isolate plugin code.
- **The filesystem sandbox has a limited scope.** It confines Bash/process filesystem access, not Lua plugins, non-Bash tools, or network access. Enforced modes currently use the macOS backend and fail with an error on unsupported platforms; they do not silently run unrestricted. Without an explicit sandbox declaration, the compatibility default is full access. See [sandbox configuration](../reference/configuration/sandbox.md) before relying on it.
- **A blocking plugin can freeze Lua-driven features.** Cancellation cannot forcibly stop every native call or misbehaving cleanup handler. Long-running synchronous delegation can also block callbacks needed by the delegated work.
- **Invalid agent tool names may produce different errors depending on how you select the agent.** CLI startup and in-session selection do not yet share all validation checks. Use exact registered tool names; see [agent configuration](../reference/configuration/agents.md).
- **Workflows run in the foreground only and do not resume.** A [`workflow`](../reference/tools/workflow.md) call blocks the parent turn until its script finishes; there is no background mode. The script's CPU budget is cooperative (instruction hooks), and a run interrupted by a crash or restart is not resumed — its members' sessions remain, but the script state is lost.
- **Persistent terminals are plain text, Unix-only in practice, and not restored.** [Terminals](../guides/terminals.md#limitations) clean output rather than emulate a screen, so full-screen programs are reported instead of shown. Only Linux detects that a command is waiting for input. Login shells report no exit codes. Terminals close when rness exits (including by `SIGKILL`, through a cleanup helper process), when idle past `rness.terminal.idle_close_secs` (off by default), and when the one-shot subagent that opened them finishes. Switching sessions leaves them open; `/terminals` and the statusline still show them. After `SIGKILL` the user's terminal settings are not restored. They run with the rness user's full authority: the filesystem sandbox does not apply to them, and a non-`danger-full-access` mode disables them instead.

## Providers and upgrades

- **“OpenAI-compatible” does not mean identical behavior.** Model IDs, reasoning options, error formats, and authentication support vary between gateways. Verify them with your provider; rness does not maintain a complete built-in catalog of model capabilities.
- **Passing local tests does not guarantee account access.** Live OAuth permissions, model availability, and remote Git authentication depend on your deployment and are not established by mock or local-fixture tests.
- **Configuration and session formats do not yet have a release-level compatibility guarantee.** Keep backups of important configuration and sessions before upgrading.

## Notes for plugin authors

These details mainly matter if you write your own plugins:

- Session/engine APIs are not available at the top level of startup configuration. Use the appropriate lifecycle callback, and do not assume a frontend session has already been selected when `ready` runs.
- `session.compact_region` is synchronous and blocks the Lua host. In a command callback, prefer `session.compact_region_async(id, opts)`: it pauses that command while allowing hooks and status updates to run. It is not a detached job API and cannot be used from ordinary hooks or status callbacks. The session remains reserved until completion; cancellation closes the paused command rather than continuing its code.
- Manual region indices are one-based, inclusive positions in the model-message view, not raw log event numbers. Use `session.compaction_view(id)` and pass its source IDs so stale selections can be rejected. Provider failures retain the existing `false`/unchanged result; service errors raise Lua errors.
- Lua execution and tool-card rendering limits are not hard timeouts. Native calls can block, and plugin code can catch instruction-hook errors. Keep callbacks bounded and provide a useful display fallback for missing or invalid card data.
- `/agent <name>` works through TUI and HTTP sends, but there is no public `rness.agents.list()` query. `rness.subagents.roster()` lists delegable roles, not every agent role or running child session.
- `rness.timer` timers are in memory only and do not survive a restart. Their 30-second callback limit counts Lua instructions, not blocking native calls. See [Lua timers](../reference/lua/timer.md).

## Scheduled reminders

- **Reminders fire only while rness is running**, and only for sessions live in that process. Reminders that come due while rness is closed become overdue until you resume the session and start a turn.
- **Use one rness process per session.** Concurrent processes on the same session can deliver a reminder twice or lose an edit. Delivery is at-least-once, so a crash can repeat one.
- **Updating rness does not update your copy of `schedule.lua`.** Older copies can drop reminders while a session is busy (compaction, commands). Re-copy it from `flavors/default/plugins/`. See [Scheduled reminders](../guides/scheduled-reminders.md).

## Documentation and reporting problems

The documentation does not yet cover every Lua API, HTTP endpoint, event type, or built-in tool. The architecture document also includes historical designs, so not every example there is an available API today.

Tests cover common rendering, plugin, and compaction workflows, not every custom renderer, conversation size, reload timing, or provider. Resize benchmarks measure rendering work only, not the full delay you may see in tmux or your terminal.

When reporting a problem, include:

- Your rness version or Git revision, operating system, and terminal/tmux version if relevant.
- The command or steps that reproduce it, what you expected, and what happened instead.
- The error message and only the relevant, redacted configuration or log excerpt.

Do not post credentials or an entire session log without checking it for private prompts, tool output, images, and workspace content.
