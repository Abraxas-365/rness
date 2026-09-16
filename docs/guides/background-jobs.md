# Background job controls

The default flavor statusline shows `1 bg job` or `N bg jobs` on the right of the statusline while jobs visible to the current session are active. The count remains visible when the model is idle or compacting. Stopping jobs remain counted until their producer settles; completed/interrupted jobs are not counted. This counts background job records, not all child-agent sessions (use `/agents` for those).

The default flavor's `jobs` plugin provides user commands that work while a turn is busy:

| Command | Action |
| --- | --- |
| `/jobs` | Open the live Lua jobs overlay, listing running and retained finished jobs. |
| `/jobs list` | Print active jobs with ID, kind, status, and command/task label. |
| `/jobs <job_id>` | Open the same overlay with that exact job selected, including finished jobs. |
| `/jobs stop <job_id>` | Request cancellation of that exact job. This is not an immediate-exit guarantee. |

The monitor keeps running and finished jobs visible together, including exit codes and cancelled jobs. Select any row and press Enter to inspect its retained output. `/jobs list` and autocomplete remain active-only, including jobs still stopping; suggestions include kind, status, and a short command/task preview.

Inspection is a **non-consuming snapshot**. It does not advance the model's `job_output` cursor, start a model turn, or wait for job completion. The overlay refreshes every 250 ms by default while visible; no polling is needed from the user. There is deliberately no implicit stop-all or ambiguous short-ID matching. Jobs owned by other sessions cannot be listed, inspected, or stopped; legacy unowned jobs retain their existing shared visibility.

## Completion delivery

Background Bash and one-shot subagent completions automatically resume an idle
owner; a busy owner receives the notice at a subsequent step boundary. There is
no three-completion limit and no need to send a user message to reset a wake
budget. Repeated settlement of a job does not notify twice, and durable job IDs
prevent duplicate delivery across restarts. Each new job can trigger another
turn, so this is not a cap on total automatic model usage. Foreground output
artifacts do not trigger completion turns.

## Monitor controls

- **j/k or Down/Up**: select a running or finished job; **Enter** opens its output.
- In detail, **j/k**, **PageUp/PageDown**, and **Home** scroll and pause following. The current bounded output snapshot is frozen so newly arriving output cannot move the text you are reading. Status metadata still refreshes.
- **End** resumes live following at the tail. **Space** toggles pause/follow.
- **Escape** returns from detail to the job list; Escape again closes the overlay.

A job stays in the monitor after it finishes, and selection stays on the same job when its status changes. The CLI persists job records and output under `<session-root>/jobs` and restores them across restarts. Processes are not resumed: abandoned running jobs recover as `interrupted`, and jobs belonging to another live instance are not imported. Custom hosts using an in-memory-only registry do not retain jobs across restarts. `/jobs` explicitly returns to the list; `/jobs <job_id>` selects a new detail and resumes following. Each session has independent selection, cursor, snapshot, and scroll state. Stopping is deliberately explicit: use `/jobs stop <job_id>`; navigation never requests cancellation.

Output is the latest **8 KiB non-consuming tail**, not an unlimited log viewer. ANSI escapes and terminal controls are sanitized; long tokens and Unicode text wrap to the available width. A paused snapshot can be older than the live tail; End replaces it with the latest snapshot. Narrow terminal windows clip the viewport without discarding the snapshot.

## Customization

The jobs plugin owns all monitor behavior in Lua. Configure it in `init.lua` before the plugins are loaded:

```lua
rness.jobs.setup {
  title = "My processes",
  refresh_ms = 500, -- integer 50..60000; default 250
  layout = {
    height = 24, -- desired panel height, 5..512
    width = 100, -- optional maximum width, centered in the overlay slot
    style = "dim",
    border = { kind = "rounded", style = "dim" },
    title_style = "title",
  },
  keys = { down = { "j", "down", "n" }, up = { "k", "up" } },
  text = {
    empty = "No jobs yet",
    list_help = "j/k/n: select | enter: output | esc: close",
  },
}
```

`rness.jobs.config = { ... }` in `init.lua` before plugin load is an alternative. `rness.jobs.setup(config)` is startup-only: configure it in `init.lua`, not in a later plugin or view/key callback. The jobs plugin snapshots those options on load and never replaces the shared setup API or mutates its configuration. Supplied values merge with defaults. For a separate plugin that owns its own monitor, explicitly use `rness.ui.replace_app` rather than reconfiguring this plugin after load. Styles use the generic app renderer's theme names or style objects, not ANSI strings.

Key actions are `up`, `down`, `page_up`, `page_down`, `home`, `follow`, `open`, `back`, and `pause`. Each accepts a key string, array of strings, or `false` to disable that action. Key arrays replace defaults. Keys use app event spellings such as `enter`, `esc`, `pageup`, `pagedown`, and `space`. The host still closes on an unhandled Escape. Help text is generated from the configured keys unless `text.list_help` or `text.detail_help` explicitly overrides it.

Text options are `list`, `empty`, `list_help`, `detail_help`, `following`, `paused`, `output`, `no_output`, `unavailable`, and `marker`. For full control of formatting, use a render callback:

```lua
rness.jobs.setup {
  render = function(ctx, model, lines)
    -- Keep the standard list/detail, replacing its heading.
    if model.mode == "list" then
      lines[1] = "Jobs: " .. #model.jobs
    end
    return lines -- nil keeps the supplied default lines
  end,
}
```

`ctx` carries `session`, `rows`, and `cols`. The model contains `mode` (`list`/`detail`), `session`, `selected`, `follow`, `offset` (zero-based wrapped-line offset), `rows`, `cols`, `page`, and `text`. List models additionally have running and retained finished `jobs` and a one-based `cursor`; detail models have `job` (fresh metadata when available), optional `error`, bounded `output` (the frozen snapshot while paused), and wrapped `body` lines.

To replace rendering entirely, provide `view = function(ctx) return { "Your lines" } end`. This bypasses the default model/inspection/render path. An optional `on_key = function(key, ctx) ... end` runs before built-in navigation: return `nil` to delegate, `true` to consume, `false` to pass to the host, or `"close"` to close. Full replacements can maintain their own Lua state. All app lines are plain strings; returned output is sanitized and capped at 512 lines, 8 KiB per line, and 64 KiB total. Custom renderers should use `rness.ui.wrap` on sanitized text if they need custom wrapping.

## Installing into an existing configuration

Updating the binary does not update copied plugins. Use the new binary, copy `flavors/default/plugins/jobs.lua` and the updated `statusline.lua` into your configuration's plugin directory, then add this entry to your existing setup list and restart:

```lua
rness.plugins.setup({
  -- Other existing plugin entries...
  { name = "statusline", file = "plugins/statusline.lua" },
  { name = "jobs", file = "plugins/jobs.lua" },
})
```

Replace the old `{ name = "spinner", file = "plugins/spinner.lua" }` entry with the `statusline` entry above; do not load both. Do not create a second `plugins.setup` call. The repository's default flavor already includes both entries. A custom statusline can use the same count API without loading the default statusline.

## Lua API

Available after the CLI mounts its shared job registry:

- `rness.jobs.count(session)` — count active, unsettled visible jobs; no output reads.
- `rness.jobs.list(session)` — job records with `job_id`, `kind`, `label`, `status`, optional `exit_code`, `running`, and `cancellation_requested`.
- `rness.jobs.inspect(session, job_id)` — the same metadata plus bounded `output` tail and `output_bytes`, without consuming output.
- `rness.jobs.stop(session, job_id)` — `true` for a newly requested cancellation, `false` if already settled or stopping. Unknown/foreign IDs raise an error.

These are trusted Lua APIs, like other session APIs. They enforce visibility for the supplied session; they are not an authentication boundary against a malicious plugin that knows other session IDs. Custom hosts must install a job registry before using them.
