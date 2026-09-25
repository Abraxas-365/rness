# Scheduled reminders

The schedule plugin lets the model set reminders that come back into the session later, for example "every 10 minutes, check whether the deploy finished" or "remind me at 15:30 to review the PR". When a reminder is due, an idle session wakes up and runs a turn by itself.

Source: [`flavors/default/plugins/schedule.lua`](../../flavors/default/plugins/schedule.lua). It is built on [`rness.timer` and `rness.session.send`/`steer`](../reference/lua/timer.md).

## Prerequisites

- rness is running. **Nothing fires while rness is closed.**
- A model that can call tools.
- Your `~/.rness/plugins/` directory contains a copy of `schedule.lua`. Existing installations are not updated automatically. If you copied it before, compare it with the current file, because older copies can delete reminders while a session is busy.

## Enable it

1. Copy the plugin into your plugins directory:

   ```sh
   cp flavors/default/plugins/schedule.lua ~/.rness/plugins/
   ```

2. Add it to `rness.plugins.setup` in `~/.rness/init.lua`. The default flavor ships this line commented out:

   ```lua
   rness.plugins.setup({
     -- ...
     { name = "schedule", file = "plugins/schedule.lua" },
   })
   ```

3. Optionally, configure it **above** `rness.plugins.setup`. Every field is optional:

   ```lua
   rness.schedule = {
     delivery = "queue",      -- "queue" (default) or "steer"; see below
     min_every_seconds = 300, -- smallest allowed every_seconds (default 300)
     tick_seconds = 5,        -- how often due reminders are checked (default 5)
     max_per_session = 50,    -- active reminders per session (default 50)
     dir = "/abs/path",       -- storage directory (default $HOME/.rness/schedules)
   }
   ```

4. Restart rness.

## Use it

Ask in natural language. The model translates the request into tool calls:

| You say | Tool call the model makes |
| --- | --- |
| "Remind me in 2 minutes to check the build" | `schedule_create { prompt, after_seconds = 120 }` |
| "Every 10 minutes, check whether the deploy finished" | `schedule_create { prompt, every_seconds = 600 }` |
| "At 15:30 Lima time, remind me to review the PR" | `schedule_create { prompt, at = "2026-09-24T15:30:00-05:00" }` |
| "What reminders do I have?" | `schedule_list {}` |
| "Cancel reminder 3" | `schedule_delete { id = "3" }` |

When a reminder is due, it enters the session as a message starting with `[SCHEDULE REMINDER]`. The prompt is JSON-encoded and marked as reminder content, not new user instructions. The model then responds to it.

## Tool reference

### `schedule_create`

| Argument | Type | Notes |
| --- | --- | --- |
| `prompt` | string, required | Non-empty, at most 4000 bytes |
| `after_seconds` | integer | Positive delay |
| `every_seconds` | integer | Recurring interval, at least `min_every_seconds` |
| `at` | string | Strict RFC 3339 with offset: `2026-09-24T15:30:00Z` or `...-05:00`. Impossible dates such as Feb 31 are rejected |

Supply exactly one of `after_seconds`, `every_seconds` and `at`. JSON `null` counts as absent. The tool returns the reminder with its `id`, the UTC `next_due` and its `state`.

### `schedule_list`

Takes no arguments. Returns the session's active reminders in creation order, each with `state` `scheduled` or `overdue`. Returns `[]` when there are none.

### `schedule_delete`

Takes `id` (string). Returns `{ id, deleted }`. `deleted` is `false` for unknown or already finished ids.

## Delivery: queue or steer

A due reminder always wakes an **idle** session. If a turn is **running** when it comes due, `delivery` decides what happens:

| `delivery` | Behavior | TUI equivalent |
| --- | --- | --- |
| `"queue"` (default) | Waits until the turn ends, then runs as its own turn. The running task is not disturbed. A recurring reminder that comes due several times during one long turn is delivered **once**, not once per missed period | Enter |
| `"steer"` | Joins the running turn at its next step boundary, so the model sees it mid-task. In-flight requests and tools are not aborted. A reminder arriving during the final step runs as its own turn | Ctrl+Enter |

Any other value makes the plugin fail to load. Its tools disappear and rness logs:

```text
plugin 'schedule' failed to load: ... rness.schedule.delivery must be "queue" or "steer", got <value>
```

## Behavior details

- **Session-local.** Reminders fire only for sessions that are live in the running rness process, meaning ones that ran a turn or used a schedule tool there. Live sessions are remembered across plugin hot reload.
- **Overdue reminders.** A reminder that comes due while rness is closed, or while its session is not live, becomes `overdue`. It fires once you resume that session and start a turn: after that turn in queue mode, during it in steer mode.
- **Recurring timing.** Recurring reminders stay aligned to their creation time and skip missed occurrences. After a long pause you get one delivery, not a burst.
- **Busy sessions.** While a session is compacting, running a command or reloading, delivery is retried on the next tick. This does not count as a failure.
- **Failures.** If delivery fails three times in a row for another reason (for example, the session was deleted), the reminder is dropped and a warning is logged.
- **Cancelled turns.** If you cancel a turn or it fails, a pending reminder stays parked with the rest of the queue. It is not run automatically.
- **Precision.** Reminders are checked every `tick_seconds`, so they can arrive up to that long after the target time.

## Storage

Each session gets one JSON file in `dir` (default `~/.rness/schedules/<session-id>.json`). Files are replaced atomically (written to a temporary file, then renamed).

- The file is kept even after its last reminder finishes, so ids are never reused within a session.
- An unreadable or corrupt file is **never overwritten**. The tools report `schedule file ... is unreadable; fix or remove it` until you repair or delete it.
- To remove every reminder for a session, delete its file while rness is closed.

## Limitations

- **One rness process per session.** Two processes on the same session (for example the TUI plus a headless `rness -p` run) can deliver a reminder twice or lose an edit.
- **At-least-once delivery.** A crash between delivering and saving can repeat a reminder.
- **Subagents.** Default subagent roles cannot use the schedule tools. If you allow them for a role, a reminder created by a subagent wakes that subagent's session, not its parent's.
- **Model behavior varies.** Whether a model calls the tools correctly and treats `[SCHEDULE REMINDER]` as a reminder depends on the model. Automated tests use a mock provider. They cover the plugin's mechanics, not real provider or model behavior.

## Verify it works

1. Ask: "Remind me in 60 seconds to stretch."
2. Check that the model called `schedule_create` and reported an id.
3. Wait without typing. Within about 65 seconds the session should start a turn by itself and mention the reminder.
4. Ask "what reminders do I have?". The list should be empty, because one-shot reminders are removed after delivery.

If nothing happens, check that the plugin loaded: its tools appear in the model's tool list, and load errors are logged at startup. Also check that the session is the one you are using in this process.

## Related

- [Lua timers and session delivery reference](../reference/lua/timer.md)
- [Queue and Steer](queue-and-steer.md)
- [Plugin loading and lifecycle](plugins/loading-and-lifecycle.md)
