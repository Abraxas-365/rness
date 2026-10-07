# Background job controls

The default flavor statusline shows `1 bg job` or `N bg jobs` on the right of the statusline while jobs visible to the current session are active. The count remains visible when the model is idle or compacting. Stopping jobs remain counted until their producer settles; completed/interrupted jobs are not counted. This counts background job records, not all child-agent sessions (use `/agents` for those).

The default flavor's `jobs` plugin provides user commands that work while a turn is busy:

| Command | Action |
| --- | --- |
| `/jobs` | Open the live Lua jobs overlay: running jobs, then recently finished ones. |
| `/jobs list` | Print active jobs with ID, kind, status, and command/task label. |
| `/jobs <job_id>` | Open the same overlay with that exact job selected, including finished jobs. |
| `/jobs stop <job_id>` | Request cancellation of that exact job. This is not an immediate-exit guarantee. |

The monitor answers "what is running now, and what just finished?". Jobs appear in two sections: **Running** (newest first) and **Recent** (most recently finished first). By default Recent holds jobs that finished in the last 30 minutes, always at least the latest 3 and never more than 10. Older jobs are summarized as `… N older hidden`; press **a** to show all of them, or **/** to search. Each row shows a colored status icon, the job kind, the label, elapsed time (or total run time once finished), how long ago it finished (`12m ago`), and the status or exit code. Labels drop a leading `cd <dir> &&`, show the working directory as `.` and your home directory as `~`, and drop the `subagent [spawn]:` prefix. The panel grows with its content, up to 24 rows. The header counts running and finished jobs and shows the current filter; the footer lists the available keys. Select any row and press Enter to inspect its retained output; the detail header shows the job ID, status, and follow state. `/jobs list` and autocomplete remain active-only, including jobs still stopping; suggestions include kind, status, and a short command/task preview.

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
- **x** stops the selected running job, from the list or the output view. The first press asks for confirmation in the footer; press **x** again to request cancellation. Any other key cancels the prompt and is not otherwise acted on.
- **f** cycles the list between recent (the default), all jobs, and running jobs only. **a** toggles between all jobs and the default filter.
- **/** starts a search across every retained job, matching label, kind, and ID. Type to narrow the list, Backspace to edit, Ctrl-U to clear. Enter keeps the results and returns the keys to navigation. Escape clears the search.
- **Escape** returns from detail to the job list, then clears an active search, then closes the overlay.

A job stays in the monitor after it finishes, and selection stays on the same job when its status changes. The CLI persists job records and output under `<session-root>/jobs` and restores them across restarts. Processes are not resumed: abandoned running jobs recover as `interrupted`, and jobs belonging to another live instance are not imported. Custom hosts using an in-memory-only registry do not retain jobs across restarts. `/jobs` explicitly returns to the list; `/jobs <job_id>` selects a new detail and resumes following. Each session has independent selection, cursor, snapshot, and scroll state. Stopping is always explicit: use `/jobs stop <job_id>` or the confirmed **x x** in the monitor. Navigation keys never request cancellation. A stopped job stays listed as `stopping` until its process exits.

Output is the latest **8 KiB non-consuming tail**, not an unlimited log viewer. ANSI escapes and terminal controls are sanitized; long tokens and Unicode text wrap to the available width. A paused snapshot can be older than the live tail; End replaces it with the latest snapshot. Narrow terminal windows clip the viewport without discarding the snapshot.

## Full-output retrieval

Output is spooled to disk with a 64 KiB in-memory tail per job. Use
`job_output({job_id = "...", offset = 0})` to page through complete output in
64 KiB byte windows without advancing the normal incremental cursor. Large
foreground Bash results also return an artifact ID (kind `bash-output`), without
sending a background completion notification. Lua-configurable quotas and age-based
cleanup bound retained output; see [execution hardening](execution-hardening.md) for settings and limits.

## Customization

The jobs plugin owns all monitor behavior in Lua. Configure it in `init.lua` before the plugins are loaded:

```lua
rness.jobs.setup {
  title = "My processes",
  refresh_ms = 500, -- integer 50..60000; default 250
  layout = {
    min_height = 5, max_height = 24, -- content-sized panel bounds, 3..28 each
    -- height = 24, -- or a fixed height, 5..512
    width = 100, -- optional maximum width, centered in the overlay slot
    style = "dim",
    border = { kind = "rounded", style = "dim" }, -- default: rounded, overlay_border
    title_style = "title", -- default: heading
  },
  list = {
    filter = "recent", -- "active" (running only), "recent", or "all"
    order = "newest", -- within each group: "newest" or "oldest" first
    group = true, -- Running / Recent section headers
    running_first = true, -- with group = false, keep running jobs on top
    recent = { secs = 1800, min = 3, max = 10 }, -- what counts as recent
    columns = { "icon", "kind", "label", "duration", "age", "status" },
    shorten_labels = true,
    kind_names = { subagent = "agent" },
  },
  -- Optional: rewrite each row's label (receives the shortened label).
  format_label = function(job, label) return label end,
  keys = { down = { "j", "down", "n" }, up = { "k", "up" }, stop = false },
  text = {
    empty = "No jobs yet",
    list_help = "j/k/n select · enter output · esc close",
  },
  styles = { running = { fg = "cyan", bold = true }, selected = "overlay" },
}
```

`rness.jobs.config = { ... }` in `init.lua` before plugin load is an alternative. `rness.jobs.setup(config)` is startup-only: configure it in `init.lua`, not in a later plugin or view/key callback. The jobs plugin snapshots those options on load and never replaces the shared setup API or mutates its configuration. Supplied values merge with defaults. For a separate plugin that owns its own monitor, explicitly use `rness.ui.replace_app` rather than reconfiguring this plugin after load. Styles use the generic app renderer's theme names or style objects, not ANSI strings.

`columns` lists the row cells in order: `icon`, `id`, `kind`, `label`, `duration`, `age`, and `status`. Cells before `label` are left-aligned; cells after it are right-aligned and stay visible when the label is clipped. Panels narrower than 60 columns drop `duration` and `age`. Arrays such as `columns` replace the defaults instead of merging with them.

Key actions are `up`, `down`, `page_up`, `page_down`, `home`, `follow`, `open`, `back`, `pause`, `stop`, `filter`, `all`, and `search`. Set `stop = false` to disable stopping from the monitor. Each accepts a key string, array of strings, or `false` to disable that action. Key arrays replace defaults. Keys use app event spellings such as `enter`, `esc`, `pageup`, `pagedown`, and `space`. The host still closes on an unhandled Escape. Help text is generated from the configured keys unless `text.list_help` or `text.detail_help` explicitly overrides it.

Text options are `list` (optional header prefix), `empty`, `empty_active`, `empty_recent`, `no_matches`, `list_help`, `detail_help`, `all_hint`, `following`, `paused`, `output` (optional detail-header label), `no_output`, `unavailable`, `marker`, `running_count`, `none_running`, `finished_count`, `filter_label`, `filter_active`, `filter_recent`, `filter_all`, `section_running`, `section_recent`, `section_finished`, `more_recent`, `more_active`, `search_prompt`, `search_cursor`, `just_now`, `ago`, `confirm_stop`, `stop_requested`, `not_running`, and `already_stopping`. `running_count`, `finished_count`, `more_recent`, `more_active`, and `ago` are `string.format` patterns. `confirm_stop` and `stop_requested` are patterns that receive the job label. Format texts are checked at setup, so write a literal `%` as `%%`. The older names `filter_running` and `empty_filtered` are still accepted as aliases for `filter_active` and `empty_active`.

Style options take a theme group name (`"added"`, `"error"`, `"dim"`, `"heading"`, …) or a style table (`{ fg =, bg =, bold =, italic =, underline =, reverse = }`): `running`, `stopping`, `ok`, `failed`, `stopped`, `kind`, `time`, `hint`, `header`, `notice`, `confirm`, `marker`, `section`, `search`, `label`, and `selected` (fill for the selected row; default `{ bg = "#3c3836" }`). For full control of formatting, use a render callback:

```lua
rness.jobs.setup {
  render = function(ctx, model, lines)
    -- Keep the standard list/detail, replacing its heading.
    if model.mode == "list" then
      lines[1] = { { text = "Jobs: " .. #model.jobs, style = "heading" } }
    end
    return lines -- nil keeps the supplied default lines
  end,
}
```

`ctx` carries `session`, `rows`, and `cols`. The model contains `mode` (`list`/`detail`), `session`, `selected`, `follow`, `offset` (zero-based wrapped-line offset), `rows`, `cols`, `page`, `text`, `now_ms`, `header`/`footer` (whether chrome rows are shown), and the pending `confirm` job ID or `notice` text. List models additionally have the sorted, filtered `jobs`, a one-based `cursor`, the display `entries` (`{ section =, count = }`, `{ job =, position = }`, or `{ more = }`) with the first visible index `top`, `filter` (`active`/`recent`/`all`), the effective `scope` (`all` while searching), the search `query` and `searching` flag, and `total`, `running`, `finished`, and `hidden` counts; detail models have `job` (fresh metadata when available), optional `error`, bounded `output` (the frozen snapshot while paused), and wrapped `body` lines.

To replace rendering entirely, provide `view = function(ctx) return { "Your lines" } end`. This bypasses the default model/inspection/render path. An optional `on_key = function(key, ctx) ... end` runs before built-in navigation: return `nil` to delegate, `true` to consume, `false` to pass to the host, or `"close"` to close. Full replacements can maintain their own Lua state. Each line is a string, a span `{ text =, style = }`, or a row: an array of strings/spans with optional `right = { spans }` (right-aligned, kept visible when the left side is clipped) and `style =` (fill for the whole row). Text is sanitized and capped at 512 lines, 8 KiB per line, and 64 KiB total; styles are theme names or style tables, never ANSI strings. Custom renderers should use `rness.ui.wrap` on sanitized text if they need custom wrapping.

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
- `rness.jobs.list(session)` — job records with `job_id`, `kind`, `label`, `status`, optional `exit_code`, `running`, `cancellation_requested`, and optional `started_at_ms`/`settled_at_ms` (Unix milliseconds; absent on records persisted by older versions).
- `rness.jobs.inspect(session, job_id)` — the same metadata plus bounded `output` tail and `output_bytes`, without consuming output.
- `rness.jobs.stop(session, job_id)` — `true` for a newly requested cancellation, `false` if already settled or stopping. Unknown/foreign IDs raise an error.

These are trusted Lua APIs, like other session APIs. They enforce visibility for the supplied session; they are not an authentication boundary against a malicious plugin that knows other session IDs. Custom hosts must install a job registry before using them.
