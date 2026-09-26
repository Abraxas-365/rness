# Terminal tools reference

Persistent PTY-backed shells for the model (`terminal_open`, `terminal_send`, `terminal_read`, `terminal_signal`, `terminal_list`, `terminal_close`), the `rness.terminals` Lua API, and the `rness.terminal` startup configuration. For usage and the `/terminals` command, see the [terminals guide](../../guides/terminals.md).

Source: [`crates/rness-tools/src/terminal.rs`](../../../crates/rness-tools/src/terminal.rs), [`crates/rness-lua/src/api/terminals.rs`](../../../crates/rness-lua/src/api/terminals.rs), [`flavors/default/plugins/terminals.lua`](../../../flavors/default/plugins/terminals.lua).

## Availability and lifecycle

- The CLI always registers the six tools, with background jobs enabled on `terminal_send`. There is no opt-in.
- All terminal state is in-process. It is not persisted and not restored after a restart.
- Terminals are **owned by the rness session** that opened them. The tools take the owner from the calling session. `rness.terminals` functions take it from their `session` argument, so a plugin can act for any session whose id it passes (plugins are trusted code). Terminals of another session are not listed to the model and can't be used by its tools. The user-facing `list_all` and `owner` functions exist so `/terminals` can show and stop them after a session switch.
- Every tool except `terminal_list` requires the session's sandbox mode to be `danger-full-access` (the default). Otherwise it fails with `terminal tools are disabled in <Mode> sandbox mode (requires DangerFullAccess)`.
- All tools need session context. Calling them without a session fails with `<tool> requires session context`.
- Ids are `term-N`, numbered from 1 per rness process and never reused.
- Signals, running/idle state, working directory, and process-tree cleanup require Unix. Input-wait detection requires Linux.

## Tools

### `terminal_open`

| Field | Type | Required | Behavior |
| --- | --- | --- | --- |
| `name` | string | No | Label shown in lists. Blank or omitted: the id is used. |
| `shell` | string | No | `"controlled"`, `"login"`, or a program path. Omitted: `rness.terminal.shell`. |

Starts the shell in the workspace root and waits up to 5 seconds for its first prompt. Returns `Opened terminal term-N (<shell label>) in <dir>.`, followed by optional lines:

- `The shell has not shown a prompt yet; check with terminal_read.` if the first prompt didn't arrive in time.
- `Withheld secret-looking env vars: A, B.` listing the names that were removed from the environment.
- `Startup output:` followed by up to 4 KiB of text printed before the prompt.

Shell labels are `bash, controlled`, `<shell>, login`, `<shell>, login, bash not found` (when controlled was asked for and no bash exists), or the program name.

Errors: `at most N terminals can be open; close one with terminal_close first (open: …)` when this session already has `max_sessions` open.

### `terminal_send`

| Field | Type | Required | Behavior |
| --- | --- | --- | --- |
| `session_id` | string | Yes | Terminal id |
| `text` | string | Yes | Text to type. Multi-line text is typed as given. |
| `submit` | boolean | No | Press Enter after `text`. Default `true`. |
| `wait_ms` | integer | No | Longest wait. Default 10000, capped at 60000. |
| `run_in_background` | boolean | No | Present only when background jobs are available. Default `false`. |

Types `text`, waits for the command to settle, then returns its clean output with the input echo removed (when `submit` is true) and one final status line:

| `outcome` | Status line |
| --- | --- |
| `exited` | `[exit code: N]` in a controlled shell; `[command finished]` if the code wasn't reported |
| `done` | `[back at the prompt; exit code unknown outside the controlled shell]` |
| `input` | `[waiting for input: reply with terminal_send to term-N, or terminal_signal to stop]` (Linux) |
| `incomplete` | `[no prompt yet: the shell wants more input …]` |
| `quiet` | `[still running, no output for 5s; it may be waiting for input. …]` |
| `timeout` | `[still running after <wait>. terminal_read to follow, terminal_signal to stop]` |
| `full_screen` | `[a full-screen program started; its screen can't be shown as text. …]` |
| `terminal_exited` | `[terminal exited (<how>); open a new one with terminal_open]` |
| `cancelled` | `[cancelled: stopped waiting; the command may still be running in term-N …]` |

Settling: a controlled shell settles on its prompt marker (an `OSC 133;D;<exit>` sequence written by `PROMPT_COMMAND`). Other shells settle 300 ms after output stops, provided they are back at the prompt. A running command that prints nothing for 5 seconds settles as `quiet`, or as `input` on Linux when it's blocked reading the terminal. A switch to the alternate screen settles as `full_screen`. None of these outcomes stops the command.

Output larger than 64 KiB keeps the tail and starts with `[earlier output omitted; terminal_read term-N with offset <n> pages from the start]`.

Cancellation of the turn stops the wait immediately and leaves the command running (`cancelled`).

**Background.** With `run_in_background: true`, the command is typed, and the call returns a job id at once. Output streams (cleaned) into a job of kind `terminal`, labelled `term-N: <first line>`, and the job completes with the command's exit code. It is delivered like any background job. `job_kill` sends INT, then TERM, then KILL to the foreground command, and the shell survives. Requirements and errors:

- the terminal must be controlled: `run_in_background needs a controlled terminal …`;
- `submit` must be true: `run_in_background runs a command, so submit must be true`;
- while the job runs, other sends fail: `terminal term-N is running background job <id>; wait for its completion, stop it with job_kill, or use another terminal`.

**Presentation.** Alongside the text result, `terminal_send` returns structured data for tool cards (`call.presentation` in `rness.ui.messagebox.tool_card`):

```json
{ "version": 1, "kind": "terminal", "terminal": "term-1", "sent": "npm test",
  "outcome": "exited", "exit_code": 0, "elapsed_ms": 3200, "status": "[exit code: 0]" }
```

`sent` is the first line of `text`, and `exit_code` is `null` unless `outcome` is `exited` with a known code. A background send has `outcome: "background"` and `job_id`, and no `elapsed_ms` or `status`.

### `terminal_read`

| Field | Type | Required | Behavior |
| --- | --- | --- | --- |
| `session_id` | string | Yes | Terminal id |
| `offset` | integer | No | Byte position in the scrollback. Omitted: the last 64 KiB. |

Returns up to 64 KiB of clean text from `offset`, then `[next_offset: N]`. When the read reaches the end, the text ends with the state first: `[term-N: idle at prompt]`, `[term-N: command running]`, or `[term-N: exited (…)]`. Pass `next_offset` later to get only new output. Scrollback keeps the latest 256 KiB; older offsets read from the oldest retained byte.

### `terminal_signal`

| Field | Type | Required | Behavior |
| --- | --- | --- | --- |
| `session_id` | string | Yes | Terminal id |
| `signal` | string or integer | No | `INT` (default), `TERM`, `KILL`, `TSTP`, `STOP`, or `CONT`; case-insensitive, with or without `SIG`, or this platform's number for one of them. |

Sends the signal to the terminal's **foreground process group**, never the shell. Waits up to 1 second and reports either `Sent SIGINT to the foreground command in term-N; it has ended or stopped and the shell is back at its prompt.` or `… it is still running after 1s. It may ignore SIGINT; try TERM, then KILL.` With no command running, `INT` clears the shell's input line (`No command is running in term-N; sent Ctrl-C to the shell to clear its input line.`), and other signals are refused, because the shell itself is never signalled.

### `terminal_list`

No input. Returns one line per terminal owned by the session, `term-N "name" (<shell label>): <state>` (the quoted name is left out when the terminal has no label), or `No open terminals.` State is `idle at prompt`, `command running`, `exited (…)`, or `open` where the platform can't tell.

### `terminal_close`

| Field | Type | Required | Behavior |
| --- | --- | --- | --- |
| `session_id` | string | Yes | Terminal id |

Ends every process in the shell's session (the foreground command, `&` jobs, the shell) with `SIGHUP`, waits up to 0.5 seconds, then sends `SIGKILL` to whatever remains. This also works after the shell has exited, since `&` jobs can outlive it. Processes that started their own session (`setsid`, self-daemonizing servers) are not reached. Returns `Terminal session term-N closed.`

### Errors

An unknown id fails with `terminal session 'term-9' not found; open: term-1 (work), term-2` or `…; none are open, start one with terminal_open`. A terminal owned by another session fails with `terminal session 'term-N' belongs to another session`, and `terminal_list` and `rness.terminals.list` never show it. A terminal rness closed on its own fails, for its owner, with `terminal session 'term-N' is closed: <reason>; open a new one with terminal_open`, where the reason is `it was closed after Ns idle (rness.terminal.idle_close_secs)` or `its subagent finished`. The most recent 64 such ids are remembered; older ones get the not-found error.

## Lua API: `rness.terminals`

Available to plugins after startup; calling it earlier fails with `rness.terminals is not installed`. It survives plugin reloads. All functions are synchronous and non-consuming: none of them advances `terminal_read` offsets or starts a model turn. `session` is the rness session id (`ctx.session`).

| Function | Returns |
| --- | --- |
| `count(session)` | `{ open, running, elsewhere, elsewhere_running }`: `session`'s terminals, and those of all other sessions |
| `list(session)` | Array of `session`'s terminal info tables, ordered by id |
| `list_all()` | Array of every open terminal's info, ordered by id. For user-facing views: the model's tools never see other sessions' terminals |
| `owner(id)` | The session id that owns terminal `id`, or `nil` if it isn't open. Pass it as `session` to act on another session's terminal for the user |
| `inspect(session, id, lines?)` | Terminal info plus `output`: the last `lines` lines of clean scrollback (default 40, max 500; `0` gives an empty string) |
| `stop(session, id)` | `true` if a command was running and stopping began, `false` if nothing was running |
| `close(session, id)` | `true` |

Terminal info fields:

| Field | Type | Present | Meaning |
| --- | --- | --- | --- |
| `id`, `name` | string | Always | `name` equals `id` when no label was given |
| `owner` | string | Always | The rness session that opened it |
| `shell` | string | Always | Shell label, such as `bash, controlled` |
| `state` | string | Always | `idle at prompt`, `command running`, or `exited (…)` |
| `running` | boolean | Always | A command, not the shell, owns the terminal |
| `uptime_secs` | integer | Always | Seconds since open |
| `command` | string | When known | The latest command line typed |
| `command_secs` | integer | When known | Seconds since that command was typed |
| `cwd` | string | macOS and Linux | Shell's working directory |
| `job_id` | string | While a background send runs | Its job id |
| `last_exit` | integer | Controlled shells, after a command | Latest reported exit code |

`stop` checks the state immediately, then runs the escalation (INT, TERM, KILL, about a second apart) on a separate thread, so it never blocks the Lua VM. `inspect`, `stop`, and `close` raise the tool's not-found error for unknown or foreign ids.

```lua
local c = rness.terminals.count(ctx.session)
if c.running > 0 then
  for _, t in ipairs(rness.terminals.list(ctx.session)) do
    if t.running then rness.terminals.stop(ctx.session, t.id) end
  end
end
```

## Configuration

`rness.terminal` in `init.lua` is read once, at startup, and changing it needs a restart. It must be a table if set, all fields are optional, and unknown keys are rejected.

| Field | Type | Default | Rules |
| --- | --- | --- | --- |
| `shell` | string | `"controlled"` | `"controlled"` or `"login"`. Program paths are allowed only per call. |
| `env_deny` | array of strings | `{}` | Extra variables to withhold, case-insensitive. Each entry is an exact name, `"PREFIX*"`, or `"*SUFFIX"`. |
| `max_sessions` | integer | `8` | Open terminals per rness session, `1`–`64`. |
| `confirm_quit` | boolean | `true` | Quitting the TUI while a terminal command runs needs a second quit within 5 seconds. |
| `idle_close_secs` | integer | `0` | `0` never closes terminals on their own. Otherwise at least `60`: a terminal closes after that many seconds with no command running, no background job, no output, and no use by id. Checked every `min(limit/4, 15 s)`. |

Validation errors stop startup, for example `rness.terminal.max_sessions must be between 1 and 64`, `rness.terminal.idle_close_secs must be 0 (off) or at least 60`, or `rness.terminal.env_deny entry "*X*" must be a name, "PREFIX*" or "*SUFFIX"`.

Built-in withholding applies regardless of `env_deny`: `API_KEY` and names ending in `_API_KEY`, `_SECRET`, `_SECRET_KEY`, `_ACCESS_KEY`, or `_PASSWORD`.

### Controlled shell environment

`bash --noprofile --norc --noediting -i` with `PS1='rness$ '`, `PS2=''`, `TERM=dumb`, `PAGER`/`GIT_PAGER`/`MANPAGER=cat`, `NO_COLOR=1`, `HISTFILE=/dev/null`, and history expansion off. `PROMPT_COMMAND` is not exported to child processes, and a script that overrides `PS1` doesn't break exit-code reporting. Every terminal, controlled or not, has `RNESS_TERMINAL=1`.

## Process cleanup

| Event | Effect |
| --- | --- |
| `terminal_close`, `rness.terminals.close`, `/terminals close` | Whole terminal session, even after the shell exited: HUP, 0.5 s, KILL |
| `/terminals stop`, `rness.terminals.stop`, `job_kill` on a terminal job | Foreground group only: INT, TERM, KILL, about 1 s apart |
| rness exits normally (any path) | Every terminal closed as above, in parallel |
| Idle for `idle_close_secs` (when set) | That terminal closed as above |
| One-shot subagent finishes | Its terminals closed as above. Continuable agents keep theirs |
| rness ended by TERM, HUP, INT or QUIT | Terminal settings restored, every terminal closed as above, then rness exits with that signal |
| rness killed (KILL) or crashed | The cleanup helper (`rness __rness-terminal-reaper`, started with the first terminal) sees its pipe close and ends every process still in the open terminals' sessions: HUP, 0.5 s, KILL. A session id is only acted on while its leader is the original shell or gone (checked by start time) |
| Process started its own session (`setsid`, daemons) | Never reached |

## Related

- [Terminals guide](../../guides/terminals.md)
- [Background jobs](../../guides/background-jobs.md)
- [System prompt sections](../../guides/system-prompt.md)
- [Sandbox modes](../configuration/sandbox.md)
