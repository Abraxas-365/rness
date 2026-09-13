# Background job controls

The default flavor statusline shows `1 bg job` or `N bg jobs` on the right of the statusline while jobs visible to the current session are active. The count remains visible when the model is idle or compacting. Stopping jobs remain counted until their producer settles; completed/interrupted jobs are not counted. This counts background job records, not all child-agent sessions (use `/agents` for those).

The default flavor's `jobs` plugin provides user commands that work while a turn is busy:

| Command | Action |
| --- | --- |
| `/jobs` or `/jobs list` | List jobs with ID, kind, status, and command/task label, including completed jobs. |
| `/jobs <job_id>` | Inspect the command/task and a bounded tail of recent output. |
| `/jobs stop <job_id>` | Request cancellation of that exact job. This is not an immediate-exit guarantee. |

Inspection is a **non-consuming snapshot**. It does not advance the model's `job_output` cursor, start a model turn, or wait for job completion. Run the command again to refresh progress. There is deliberately no implicit stop-all or ambiguous short-ID matching. Jobs owned by other sessions cannot be listed, inspected, or stopped; legacy unowned jobs retain their existing shared visibility.

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
