# TUI commands and completion

Type `/` at the beginning of the input to open completion above the editor. Matching is by name prefix, not fuzzy search. Up/Down or Ctrl-N/Ctrl-P move the selection. Tab or Enter accepts a candidate without submitting it. Argument completion opens when candidates exist. Enter with the picker closed submits the input. Escape closes the picker without clearing the draft.

## Commands

| Input | Behavior |
| --- | --- |
| `/jobs`, `/jobs <job_id>` | With the default jobs plugin: open the live monitor or a job's non-consuming output, including retained finished jobs, even while busy. See [background jobs](../background-jobs.md). |
| `/jobs list`, `/jobs stop <job_id>` | Print active jobs or request cancellation of an explicit job. |
| `/terminals`, `/terminals <term_id>` | With the default terminals plugin: open the terminals monitor, or a terminal's live, non-consuming output, even while busy. See [persistent terminals](../terminals.md). |
| `/terminals list`, `/terminals stop <term_id>`, `/terminals close <term_id>` | Print this session's terminals, interrupt a terminal's running command (INT, then TERM/KILL; the shell stays), or close the terminal and everything started in it. |
| `/agents`, `/agents steer <id> <message>`, `/agents stop <id> [confirm]` | With agent controls: inspect descendants, steer, or request a confirmed stop. ID-first forms are also supported. See [subagent commands](../subagent-commands.md). |
| `/agent <name>` | Select a declared role in an idle session, persisting its configuration without a model turn. |
| `/skill <name> <input>` | Append skill instructions to the submitted content while preserving the original text. |
| `/<skill-name> <input>` | Shorthand for a discovered skill; use `/skill` when a name conflicts with a command. |
| `/unload <name>` | Unload a named plugin under engine maintenance. Does not uninstall it. |
| `/colorscheme <name>` | Change the local TUI palette immediately. Does not modify startup configuration. |

Agent and skill candidates are discovered when the TUI is constructed. Loaded-plugin candidates are refreshed from the VM roster by polling every 100 milliseconds; an already queued operation may delay the response. A disappearing candidate can still be typed manually, and execution checks the current state. The catalog is not a general Lua command-registration API.

## Skills and HTTP

Skills are discovered in project `.rness/skills` and user `~/.rness/skills`; the project wins name conflicts. Skill loading is repeated when a request is submitted. No `$ARGUMENTS` substitution occurs. The skill body and its resource directory are appended as another text part; the original input remains intact. Unknown explicit `/skill` requests fail. Unknown shorthand names remain ordinary text.

The CLI installs this resolver in `SessionService`, shared by TUI and HTTP sends. Custom service compositions must install their own input resolver to enable skill expansion. `/agent` is handled directly by the service for a request containing exactly one text part. For example, send this JSON to `POST /api/request` with a real session ID:

```json
{
  "type": "send",
  "session": "SESSION_ID",
  "intent": "followup",
  "content": [{ "kind": "text", "text": "/agent architect" }]
}
```

A successful role command returns the existing `log_only` disposition. It records a request-configuration event, not a user message. `/unload` and `/colorscheme` are local presentation/host commands, not HTTP slash commands.

## Errors and input history

The local backend acknowledges sends before displaying a user entry. Rejected sends produce notices rather than optimistic user messages. Successful role commands also produce a local notice. Asynchronous provider failures are separate from submission rejection.

Up/Down traverse input submitted during this TUI execution and restore the draft at the end. Consecutive identical submissions are deduplicated. Commands and unsuccessful submissions remain in input history. An open picker takes priority; multiline drafts retain vertical cursor movement. History is not persisted across restarts.

See [agents](../../reference/configuration/agents.md), [plugin lifecycle](../plugins/loading-and-lifecycle.md), and [colorschemes](colorschemes.md).
