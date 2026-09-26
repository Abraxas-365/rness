# Persistent terminals

rness gives the model persistent terminals: shells that stay alive between tool calls, so a dev server, a REPL, or an interactive prompt keeps its state. This guide covers when the model uses them, how to read their results, how you watch and stop them, and how to configure them.

For exact tool fields, the Lua API, and configuration types, see the [terminal tools reference](../reference/tools/terminal.md).

## When a terminal is the right tool

| Need | Use |
| --- | --- |
| A one-shot command (`cargo test`, `git status`, a build) | Bash |
| A long one-shot command you don't want to wait on | Bash with `run_in_background` |
| A process that must keep running while the model does other work (a dev server, a file watcher) | A terminal |
| State that must carry between commands (an activated virtualenv, `cd`, exported variables, a database shell, a Python REPL) | A terminal |
| A program that asks questions (`npm init`, a confirmation prompt) | A terminal |
| A full-screen program (vim, htop, less) | Neither. Use a plain command instead |

The default flavor's `tool:terminal` [system-prompt section](system-prompt.md) tells the model the same thing: prefer Bash for bounded commands, track terminal ids, close terminals it no longer needs, and stop stuck commands with `terminal_signal`.

## Prerequisites

- Unix (macOS or Linux). The tools build on other platforms, but signals, the running/idle state, and cleanup of child processes need Unix.
- The session's sandbox mode must be `danger-full-access`, which is the default. In any other [sandbox mode](../reference/configuration/sandbox.md) every terminal tool except `terminal_list` fails with `terminal tools are disabled in … sandbox mode`.
- `bash` at `/bin/bash`, `/usr/bin/bash`, `/usr/local/bin/bash` or `/opt/homebrew/bin/bash` for the controlled shell. Without it, terminals fall back to your login shell (see [below](#shells)).
- For `/terminals`, the statusline count and the tool cards, the default flavor's `terminals` and `statusline` plugins (see [installing into an existing configuration](#installing-into-an-existing-configuration)).

## Reading a result

Every `terminal_send` result is the command's output, with the echoed input removed, followed by **one status line**. The model is expected to act on that line:

| Status line | Meaning |
| --- | --- |
| `[exit code: N]` | The command finished and the shell is back at its prompt. Controlled shell only. |
| `[back at the prompt; exit code unknown outside the controlled shell]` | The command finished in a login shell or another program. |
| `[still running after 10s. …]` | `wait_ms` passed and the command is still going. This is normal for servers. |
| `[still running, no output for 5s; it may be waiting for input. …]` | The command went quiet. It may be waiting for input (on macOS rness can't tell). |
| `[waiting for input: …]` | Linux only: the command is blocked reading the terminal. Reply with another `terminal_send`. |
| `[no prompt yet: the shell wants more input …]` | The shell is still reading, for example after an unclosed quote. Send the rest, or interrupt with `INT`. |
| `[a full-screen program started; …]` | The program switched to the alternate screen, which can't be shown as text. Quit it. |
| `[terminal exited (…); open a new one with terminal_open]` | The shell itself ended. |
| `[cancelled: stopped waiting; the command may still be running in term-N …]` | You cancelled the turn. See [cancelling](#cancelling-a-turn). |

A quiet or timed-out result does **not** mean the command stopped. `terminal_read` shows what arrived later and ends with the terminal's state, such as `[term-1: idle at prompt]` or `[term-1: command running]`.

The output is cleaned before the model sees it. Colours and cursor escapes are removed, a progress bar rewritten with carriage returns collapses to its final state, and backspaces are applied.

## Watching and stopping terminals

With the default flavor's `terminals` plugin, these commands work even while the model is busy:

| Command | Action |
| --- | --- |
| `/terminals` | Open the terminals monitor: a list of this session's terminals. |
| `/terminals <term_id>` | Open the monitor on that terminal's live output. |
| `/terminals list` | Print each terminal's id, name, shell, state, and current command. |
| `/terminals stop <term_id>` | Stop the command running in that terminal. The shell stays open. `/terminals kill <term_id>` is the same. |
| `/terminals close <term_id>` | Close the terminal, ending its shell and everything started from it. |

Completion offers the open ids, with `stop` shown only for terminals that are running a command.

`/terminals list` prints lines like:

```text
Terminals in this session:
term-1 dev [bash, controlled] running 2m14s — npm run dev
term-2 [bash, controlled] idle (last exit 1)
```

**Stop** sends Ctrl-C (`INT`) to the foreground command. If the command is still running after about a second, rness sends `TERM`, then `KILL`. It only targets the foreground command. The shell survives, and so do jobs that the command line started in the background with `&`. The command returns at once; the escalation happens in the background.

**Close** ends the shell and every process in its terminal session, including `&` background jobs. Processes get `SIGHUP`, then `SIGKILL` after 0.5 seconds.

Looking at a terminal never changes what the model sees. The monitor, `/terminals`, and the Lua API read snapshots, and they don't advance the model's `terminal_read` position.

### Monitor controls

- **j/k** or **Down/Up**: select a terminal. **Enter** opens its output.
- In the output view, **j/k** and **PageUp/PageDown** scroll and pause following. **End** resumes following the tail.
- **s** stops the selected terminal's command. **x** closes the terminal.
- **Escape** returns from the output to the list. Escape again closes the monitor.

The monitor refreshes every 500 ms and shows the terminal's working directory (macOS and Linux) and uptime.

### Statusline and tool cards

The default statusline shows `1 term` or `N terms` while terminals are open in the session, and `N terms (M running)` in a brighter colour while any of them is running a command.

The `terminal_send` card shows the command, the last 12 lines of output, and the outcome on the right: `exit 0` in green, `exit 1` in red, and `still running`, `waiting for input`, or `background` highlighted. `terminal_open`, `terminal_signal`, and `terminal_close` get one-line cards.

## Cancelling a turn

If you cancel the turn while the model is waiting on `terminal_send`, the tool stops waiting straight away but **does not kill the command**. The result says `[cancelled: … may still be running in term-N …]`, so both the model and the transcript show it's still running. To stop it, use `/terminals stop <term_id>` or ask the model to.

## Background commands

When background jobs are available (always, in the CLI), `terminal_send` accepts `run_in_background: true`. It returns a job id immediately; the output streams into the job, and completion is delivered like a background Bash job, with the exit code. The job appears in `/jobs`, and `job_kill` interrupts it (INT, then TERM, then KILL) without closing the terminal. While the job runs, the terminal is busy and other sends to it fail.

This needs a controlled terminal, because rness needs the shell's prompt marker to know when the command ends.

## Quitting

If a terminal command is running when you quit (Ctrl-D, or Ctrl-C while no turn is running; both are rebindable via `rness.keymaps`), rness doesn't exit straight away. It shows:

```text
1 terminal command is still running (term-1: npm run dev). Quit again within 5s to stop them and exit, or /terminals to manage this session's.
```

The list covers terminals in every session of this rness process; `/terminals` shows only the current session's.

Quit again within 5 seconds to exit. On exit, rness closes every terminal and ends every process started in it, including `&` background jobs, even when the shell itself already exited. Closing all terminals runs in parallel, so exit waits at most one 0.5 s grace period. This happens on every normal exit path, including headless runs (which never prompt) and startup errors. Set `confirm_quit = false` to skip the prompt.

Cleanup finds processes by their session id. `nohup` and `disown` don't change it, so those processes are still ended. Programs that start a new session on purpose (`setsid`, self-daemonizing servers) are out of reach, and so are processes started through another service, such as `tmux new -d` or `docker run -d`. rness doesn't stop them.

If rness itself is killed by a signal, it can't clean up; the shells then receive the terminal hangup when their PTY closes, which ends most programs but not ones that ignore `SIGHUP`.

## Shells

By default, terminals run a **controlled shell**: `bash --noprofile --norc` with a fixed prompt (`rness$ `), no history file, `TERM=dumb`, `NO_COLOR=1`, and pagers set to `cat`. Its prompt hook reports each command's exit code to rness. Your own aliases and dotfiles are not loaded.

`shell = "login"` (per call, or as the default in config) runs your `$SHELL` as a login shell with your dotfiles. Exit codes are then unknown, and a customised prompt can make completion detection less reliable. Any other `shell` value runs that program directly.

Environment variables that look like credentials are withheld from every terminal. That means `API_KEY` and names ending in `_API_KEY`, `_SECRET`, `_SECRET_KEY`, `_ACCESS_KEY`, or `_PASSWORD`, plus anything matching `env_deny`. `terminal_open` lists the withheld names. Every terminal has `RNESS_TERMINAL=1` set.

## Configuration

Set `rness.terminal` in `init.lua`. It's read once, at startup:

```lua
rness.terminal = {
  shell = "controlled",               -- or "login"
  env_deny = { "AWS_*", "*_TOKEN", "DATABASE_URL" },
  max_sessions = 8,                   -- per rness session, 1..64
  confirm_quit = true,
  idle_close_secs = 0,                -- 0 = never; else at least 60
}
```

With `idle_close_secs` set, a terminal closes on its own once it has gone that long with no command running, no background job, no output, and no use by id (`terminal_send`, `terminal_read`, `terminal_signal`, or the `/terminals` stop, close, and monitor views). The model gets `terminal session 'term-N' is closed: it was closed after 600s idle (rness.terminal.idle_close_secs)` if it uses one later.

Terminals opened by a one-shot subagent close when the subagent finishes, since nothing can use them afterwards. A continuable agent keeps its terminals between turns.

Every field is optional. Unknown keys and invalid values stop startup with an error naming the field. See the [reference](../reference/tools/terminal.md#configuration) for exact rules.

## Installing into an existing configuration

Updating the binary does not update copied plugins. Copy `flavors/default/plugins/terminals.lua` and the updated `statusline.lua` into your configuration's plugin directory, add this entry to your existing setup list, and restart:

```lua
rness.plugins.setup({
  -- Other existing plugin entries...
  { name = "terminals", file = "plugins/terminals.lua" },
})
```

The terminal tools, the quit prompt, and cleanup on exit work without the plugin. The plugin adds `/terminals`, the monitor, and the tool cards.

## Verification

1. Ask the model to start a dev server in a terminal (for example `python3 -m http.server 8123`).
2. The statusline shows `1 term (1 running)`, and the card shows `still running`.
3. `/terminals list` shows the command. `/terminals term-1` shows its live output.
4. `/terminals stop term-1` returns the statusline to `1 term`.
5. Start the server again and quit. The first quit shows the prompt, and the second exits. Afterwards `pgrep -f http.server` finds nothing.

For an automated version, `python3 scripts/terminal_acceptance.py` runs the built binary in tmux with the default flavor, a throwaway HOME, and a scripted local mock model, so it needs no credentials. It needs tmux and bash. It checks the cards, exit codes, input waits, the statusline, `/terminals`, the monitor, stop, cancel, that a one-shot subagent's terminal closes when it finishes, the quit prompt, and that no process survives quitting.

## Limitations

- **No terminal emulation.** Output is cleaned text, not a screen. Full-screen programs (vim, htop, less, `top`) are detected and reported instead of shown. Programs that redraw with cursor movement can still produce odd text.
- **Input detection on macOS.** Only Linux can see that a command is blocked reading the terminal. On macOS a silent command reports `still running, no output for 5s; it may be waiting for input`.
- **Login shells** report no exit codes, and heavily customised prompts (zsh themes, `precmd` hooks) can delay or confuse completion detection.
- **Not persisted.** Terminals are processes of the running rness. They are not restored after a restart. They close when rness exits, when they're closed explicitly or go idle (`idle_close_secs`), or when the one-shot subagent that opened them finishes. rness has no session deletion, and switching sessions leaves terminals open.
- **Scrollback** keeps the latest 256 KiB per terminal. A single send or read returns at most 64 KiB.
- **Idle timeout is off by default.** Without `idle_close_secs`, terminals stay open until closed. `max_sessions` caps how many one rness session can have open.
