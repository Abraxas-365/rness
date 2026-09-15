# Opt-in session query plugins

Copy `examples/plugins/session-search-sqlite.lua` and
`examples/plugins/session-search.lua` into your config's `plugins/` directory.
Add these entries to your **existing** `rness.plugins.setup` list:

```lua
{ name = "session-search-sqlite", file = "plugins/session-search-sqlite.lua" },
{ name = "session-search", file = "plugins/session-search.lua",
  dependencies = { "session-search-sqlite" } },
```

Neither is enabled by default. Provider-only activation exposes a plugin API,
not model tools. Loading the provider does not create/open SQLite or scan logs.
The first valid search creates `session-search-v2.sqlite3` in the session store.
Event reads and traces read JSONL without opening SQLite. JSONL remains the
source of truth. The prototype `session-search.sqlite3` is left untouched.
SQLite is included in the binary; no Cargo feature/rebuild is needed to opt in.

## Tools

- `session_search`: one strongest event per matching session, not one result per
  message. Includes the current session's committed history.
- `session_event_search`: individual events within one session (current by
  default). Both searches accept `query`, `session_id`, `limit` (1–100, default
  20), `cursor`, `event_types`, `surfaces`, and inclusive RFC3339 `time_from` /
  `time_to`. Queries are literal token phrases, not arbitrary FTS expressions.
- `session_event_read`: exact event JSON in chunks of up to 8192 Unicode
  characters. Supply `event_ref` and optionally `session_id`, then concatenate
  `chunk` fields using `next_cursor`. An individual chunk need not be valid JSON.
- `session_event_trace`: direct compaction replacement/source links to or from
  an event. Takes `event_ref`, optional session, limit, cursor.
- `session_trace`: the fork reference plus local compaction/source links. Takes
  optional session, limit, cursor. Neither trace recursively opens other sessions.

Search/trace pages return `{items, next_cursor}`; a null cursor ends pagination.
Keep all arguments unchanged when following a cursor. Search cursors bind the
caller, workspace, operation and filters to a bounded result snapshot (up to 1000
hits, eight snapshots per provider, ten-minute expiry). `snapshot_truncated`
indicates that more than 1000 hits matched; narrow the query to retrieve others.
Snapshots survive intervening tool/message appends, and target workspace access
is rechecked on each page. They do not reflect newly appended content until a new
search. Expired/evicted/reloaded cursors fail explicitly; restart without them.
Event-read cursors bind the addressed event content, and trace cursors bind the
relationship list, so unrelated messages do not invalidate them. Search snippets
are capped at 512 characters each; at most 100 hits per page.

## Tool card styling

The tools plugin includes themed cards for all five tools. They inherit your
existing `rness.ui.messagebox.tool` border, background, padding and preview
settings, so a Gruvbox/rounded-card theme needs no separate palette. Cards show
query/scope, result counts, short snippets and event references, pagination hints,
read-chunk previews and trace links. Errors use `error`; running states use
`heading`. Previews are UTF-8-aware, bounded, and do not modify model tool output.

To give these cards more room, merge these entries into your **existing**
`rness.ui.messagebox.tools` table in your theme (do not replace the whole theme):

```lua
session_search       = { display = "preview", preview_lines = 12 },
session_event_search = { display = "preview", preview_lines = 12 },
session_event_read   = { display = "preview", preview_lines = 8 },
session_event_trace  = { display = "preview", preview_lines = 10 },
session_trace        = { display = "preview", preview_lines = 10 },
```

The default renderer uses semantic colors: `tool_name` for titles and matches,
`heading` for queries, `dim` for identifiers, `added` for completion/pagination,
and `code` for JSON previews. Edit those colors in your colorscheme to coordinate
all cards, or edit `card(call)` in your copied `plugins/session-search.lua`.
It previews four results and abbreviates snippets to 220 characters; change the
`math.min(#items, 4)` loop and the matching remaining-count check together if you
want more. Card previews intentionally do not display full cursor tokens.

For a complete custom card, set `render = function(call) ... end` on a per-tool
entry above. User theme renderers take precedence over plugin defaults. Example:

```lua
session_search = {
  display = "preview", preview_lines = 12,
  render = function(call)
    if call.is_error then return nil end -- use standard error rendering
    local ok, data = pcall(rness.json.decode, call.output or "")
    if not ok or type(data) ~= "table" then return nil end
    local items = type(data.items) == "table" and data.items or {}
    return {
      header = {
        left = { { text = "History search", style = "tool_name" } },
        right = { { text = #items .. " matches", style = "added" } },
      },
      body = { { text = "Saved history · workspace scoped", style = "dim" } },
    }
  end,
},
```

Prefer semantic styles instead of hardcoded colors, handle running/error/empty
results, and keep renderers read-only: no database queries or file access from
card callbacks. Rendering should remain cheap while scrolling.

## Indexing and execution

Each search enumerates session headers and stats log files, but only **changed
sessions** have their bodies read and SQLite rows replaced. Deleted/moved sessions
are removed on refresh. A changed session is reindexed as a unit so compaction
can update earlier events' visibility. This is per-session incremental indexing,
not incremental FTS insertion for every appended line. Unchanged bodies are not
read. Revision tracking uses file size/mtime, plus device/inode/ctime on Unix.

The Lua tool yields while a Tokio blocking worker performs authorization,
filesystem reads, indexing and querying. The VM can service status/UI and other
calls meanwhile. One operation per provider runs at a time; concurrent attempts
receive a retryable busy error rather than queueing blocking workers. Responses
have a 60-second timeout. **Already-running blocking work is not forcibly stopped
by timeout or cancellation** and may complete its index transaction afterward.
Reload/unload is rejected while a query coroutine is pending.

Workspace identity comes from the engine's caller session, not tool arguments.
Targets in another workspace and path-like session IDs are rejected. The local
session store/configuration is trusted; this is not a multi-tenant OS sandbox.
The versioned derived database is checked for application ID/schema version;
unknown databases are refused instead of overwritten. Failed refreshes do not
silently serve stale results.

## Coverage and remaining differences from deepseek-harness

Search extracts string content from durable events (messages, reasoning,
tool calls/results, compaction and audit records), excluding encoded image data,
URLs and duplicated streaming chunks. Exact event reads include original content.
Surfaces use the existing model-context/compaction projection: `current`,
`shadowed`, or `log-only`, evaluated for the source-local session history.

Events are indexed **once under their original session**, not copied into every
fork. Search/read a cited parent source session explicitly, subject to workspace
checks. There is no separate unsaved live-event overlay; committed JSONL appends
are picked up at the next search. Traces expose explicit fork and compaction
links, not full harness provenance, tool-call graphs or recursive ancestry.
Session creation/parent/availability filters, neighboring-event windows, byte-
accurate spill files and pluggable provider selection UI are not implemented.

Large initial indexing still takes time and memory on a worker. Full-event reads
bound returned output, not the memory needed to decode the source log.

## Performance checks

Reproduce the synthetic probe with:

```sh
cargo test --release -p rness-engine --test session_query_performance -- --ignored --nocapture
```

The default corpus has 100 sessions × 1000 events (~90.65 MiB of JSONL).
Override `RNESS_BENCH_SESSIONS` and `RNESS_BENCH_EVENTS` for other shapes.
On an Apple M3 Pro, the optimized query measured approximately 231 ms median for
an all-events common-term grouped search (previously 736 ms), 31 ms for a scoped
common-term search (previously 79 ms), and 31 ms for one append plus refresh
(previously 72 ms). Initial indexing took 1.57 s (previously 3.90 s). Index size
increased from 143.5 to 153 MiB. These are synthetic warm-filesystem observations,
not latency guarantees or cold-disk measurements; no million-event run yet.

Search ranks/selects before generating bounded snippets, batches snippet lookup,
and uses indexed scope metadata for filtering and session deletion. Schema v2
indexes are transactionally upgraded to v3 in the same file on first search;
existing FTS rows and JSONL remain intact. Changed sessions still rebuild as a
unit; append-only row indexing and event-offset reads are future optimizations.
