# Execution hardening

## Tool scheduling

Native calls and Lua `tools.parallel` run consecutive concurrency-safe calls in
parallel, with exclusive barriers before/after mutating or unknown tools. Rust
implementations opt in through `Tool::concurrency_safe`; Read, Glob, and Grep opt
in. Other tools, including Lua and MCP tools, are exclusive by default. Results
remain in model order. Cancellation prevents queued calls from starting.

This is **batch-local scheduling**, not a transaction or a cross-session lock.
Already-running background commands, other sessions/processes and trusted plugins
can still modify files concurrently. Freshness checks do not eliminate hostile
filesystem races.

## File policy

Write/Edit now honor the same durable sandbox mode as Bash: read-only denies
mutations, workspace-write checks canonical containment (including existing
ancestors of new files), and full access is unchanged. Restricted writes reject
parent traversal, dangling/escaping symlinks and existing multiply linked files
on Unix. Reads remain unrestricted. Path checks are not an OS sandbox and do not
protect against a hostile process swapping directories between check and use.
Bash confinement uses macOS Seatbelt or Linux Bubblewrap. On Windows, restricted
commands require Docker Desktop Linux containers and an explicitly configured
local image; they run a Linux shell with only the workspace mounted, not native
Windows processes. Missing backends fail closed. Windows-native AppContainer
confinement is not implemented.
Lua/native extensions and MCP servers remain trusted code outside this confinement.

## MCP

Stdio children inherit only PATH, HOME, USERPROFILE, SYSTEMROOT, WINDIR, PATHEXT,
TEMP, TMP, TMPDIR, LANG and LC_ALL. Other values, including credentials and proxy
configuration, must be supplied explicitly in `env`. This does not prevent a
trusted server from reading credential files under HOME.

Requests have a deadline covering lock acquisition, writes and responses. Dropped,
cancelled, timed-out and failed requests remove their pending entry. Cancellation
or timeout during transmission closes the connection rather than reusing a partial
frame. Incoming and outgoing newline-delimited frames are bounded at 16 MiB;
malformed/oversized incoming frames close
the connection. Catalogs have 4096-tool and 128-cursor bounds; duplicate tool names
and registry conflicts are errors instead of panics. Registrations are removed
only while still owned by that connection.

The Lua host watches `notifications/tools/list_changed`, refreshes tools and
preserves deferred exposure. EOF unregisters stale tools and terminates the child.
Reconnect is available explicitly via `rness.mcp.reconnect("name")`. Optional Lua
`reconnect` policy enables bounded exponential backoff. Both paths restart,
handshake and rediscover without replaying tool calls. Restarting a stdio server
can repeat its startup side effects; automatic reconnect is disabled by default.
Connections and retry budgets survive Lua reloads.

Streamable HTTP supports authenticated POST requests, JSON and bounded SSE
responses, protocol/session headers, and DELETE session termination. Redirects
and ambient proxies are disabled. HTTPS is required except on loopback.
Optional standalone GET notifications refresh tool catalogs without an active
call. Both GET streams and interrupted POST SSE responses support event-ID
resumption through GET with `Last-Event-ID`; the original POST is never replayed.
Both options are disabled by default.

Configure from a post-mount Lua plugin (`rness.mcp` is not mounted in init.lua):

```lua
rness.mcp.connect {
  name = "remote",
  url = "https://mcp.example.com/mcp", -- or command/args/env for stdio
  headers = { Authorization = "Bearer " .. os.getenv("MCP_TOKEN") },
  timeout_ms = 30000,
  defer_tools = true,
  sse = { -- HTTP only; notifications and resume default to false
    notifications = true,
    resume = true,
    max_attempts = 5,
    retry_delay_ms = 500,
    idle_timeout_ms = 60000,
  },
  reconnect = {
    enabled = true,
    initial_delay_ms = 500,
    max_delay_ms = 30000,
    max_attempts = 5,
  },
}
```

The retry budget is per explicit connect (not reset after a short-lived successful
handshake). Disconnect cancels retries. Calls with uncertain outcomes are never
replayed; callers must decide whether retrying a mutation is safe.

SSE recovery has its own bounded budget: `max_attempts` (0–100) per POST stream
or per standalone stream lifetime. `retry_delay_ms` (1–60000) is a fixed local
retry delay; server `retry:` hints do not override it. `idle_timeout_ms`
(1–86400000) bounds silence between chunks. Standalone GET streams can outlive
`timeout_ms`; POST response recovery still shares the original call deadline.
A POST stream cannot resume without an event ID. Standalone streams without an
ID may reopen, but cannot recover missed notifications. HTTP 405 on an initial
GET leaves POST usable; HTTP 404 closes the connection without reinitializing
or replaying calls. SSE events are individually limited to 16 MiB, event IDs to
1024 bytes. Unsupported server requests receive JSON-RPC method-not-found;
`ping` is supported. Disconnect cancels stream readers and pending HTTP recovery.
Tool cancellation sends a bounded best-effort `notifications/cancelled`; it does
not guarantee remote side effects stopped.

## HTTP serving

For `rness --serve ADDR`, non-loopback listeners require `RNESS_SERVER_TOKEN`.
When set, the token must contain at least 32 non-space printable ASCII characters;
generate a random secret rather than a memorable password. Clients must send
`Authorization: Bearer <token>` on **every** request, including SSE. Tokens are not
accepted in URLs. Local loopback use can omit the token. Browser Origin headers
and non-navigation Fetch Metadata requests are rejected, including on loopback.
Tokenless requests also require a loopback IP or `localhost` Host/authority;
arbitrary hostnames are rejected to prevent DNS rebinding.

Use TLS termination or an SSH tunnel for remote connections: bearer authentication
does not encrypt traffic. This is single-user authentication, not tenant isolation.
Custom Rust hosts must explicitly install `auth::authorize` middleware; the raw
`router` remains a composition API. No CORS browser-client support is supplied.

## Output retention

Bash drains both pipes with bounded tails while spooling their complete bytes to
an owner-scoped job artifact. Large foreground results include the artifact ID;
background truncation includes retrieval instructions. Foreground artifacts use
kind `bash-output` and do not produce completion notifications. Full spool output
interleaves streams in arrival order; inline foreground previews remain stdout
then stderr.

`job_output({job_id = "...", offset = 0})` reads up to 64 KiB from the full spool.
Use the returned next byte offset to page; explicit offset reads do not move the
ordinary incremental cursor. Text decoding is lossy for non-UTF-8 bytes and pages
may split multi-byte characters. The disk file retains the original bytes.
Job ownership checks apply to both reading modes. In-memory output is bounded to
64 KiB per job; recovery reads only the tail. Once a job is settled and its
completion delivered (always true for foreground captures), its tail leaves RAM
and readers fetch it from storage. Custom nonpersistent hosts keep output of up
to 64 KiB in memory and spill larger output to anonymous temporary files,
removed when their registry/jobs are dropped.

The CLI retains artifacts under `<session-root>/jobs` across restarts, including
foreground captures. Retention is **bounded by default** in the core; these are
the defaults (the shipped flavor states the byte/age values explicitly in
`flavors/default/init.lua`):

```lua
rness.jobs.setup {
  retention = {
    max_job_bytes = 256 * 1024 * 1024,
    max_total_bytes = 2 * 1024 * 1024 * 1024,
    max_age_secs = 7 * 24 * 60 * 60,
    max_bash_captures = 200,
    cleanup_interval_secs = 60,
  },
}
```

`max_bash_captures` keeps the newest settled foreground captures (`bash-output`)
and evicts older ones, both on the cleanup tick and as new captures arrive.
Zero disables the corresponding byte limit, age-based deletion or capture cap. Cleanup interval
must be 1–86400 seconds. Quotas count output bytes owned/recovered by this host,
not metadata or other live instances. Running jobs, outstanding readers and
undelivered owned completions are protected. Expired settled artifacts are
removed; interrupted deletions are reconciled on recovery. When a write would
exceed `max_total_bytes`, the oldest settled artifacts that are not protected
(delivered or never owed a notice, no reader) are evicted first to make room.
Only when nothing is evictable does the full quota cancel the producer and
expose an explicit incomplete-output error. `max_job_bytes` never evicts. Disk
failures also cancel producers.
Background job count is not capped; treat artifacts as sensitive.

## Bash process cleanup

Each Bash command runs in its own session (`setsid`). rness registers that
session with the cleanup helper that terminals use (`rness
__rness-terminal-reaper`, started on first use). If rness dies in any way,
including `kill -9`, OOM or abort, the helper ends every process still in a
registered session: background jobs, foreground commands and their `&`
children (`SIGHUP`, 0.5 s, `SIGKILL`). A command's `&` children that outlive
it are also ended when rness exits normally. When a command's shell exits,
rness tells the helper, which records the processes left in the session (pid
and start time) and forgets the session if none are left. A session is only
ended while it is provably still the same one: its shell is alive with the
start time recorded at registration, or one of the recorded processes is
still in it. A session id whose shell is gone with nothing recorded may have
been reused by another program, for example another rness's command, so it is
never touched. The helper also drops sessions with nothing left to end every
second. Background job records store the
session id and the leader's start time. If the helper died too, the next rness
start kills the session when the leader is still the same process. It also
marks the newest crashed run's jobs `interrupted` synchronously (150 ms budget)
and leaves the rest to background recovery. Processes that start their own
session (`setsid`, daemons that double-fork) are out of reach. Windows has no
equivalent yet; a Job Object is the planned mechanism.

## Process backend configuration

```lua
rness.sandbox.setup {
  default = "workspace-write",
  process = {
    unix_shell = "/bin/sh",
    macos_runner = "/usr/bin/sandbox-exec",
    linux_runner = "/usr/bin/bwrap",
    -- Windows unrestricted shell:
    windows_shell = "powershell.exe",
    -- Windows restricted commands (Docker Desktop, Linux containers):
    windows_container_runner = "docker.exe",
    windows_container_image = "your-local-dev-image:tag",
    windows_container_shell = "/bin/sh",
    windows_container_pids = 256,
  },
}
```

Linux requires usable user namespaces and Bubblewrap. Windows images must already
be available (`--pull=never`), include the selected shell and tools, and understand
`/workspace` paths. Only the workspace is host-mounted, read-only in read-only
mode. Containers have a read-only root, dropped capabilities, no-new-privileges,
and a process limit. Network is not isolated. This is container-backed Windows
support, not native Windows-command confinement. macOS retains its native
Seatbelt behavior; `process.temp_parent` optionally selects its private-temp parent.

### Linux validation in Docker

The Linux launcher's exact Bubblewrap arguments were exercised on Docker's
Linux 5.15.49-linuxkit-pr kernel with Debian Bookworm and Bubblewrap 0.8.0,
using an unprivileged UID. The disposable test container required
`--cap-add SYS_ADMIN --security-opt seccomp=unconfined --security-opt systempaths=unconfined`
to allow nested namespace/proc mounts. It had no host-directory mounts or Docker
socket. These elevated Docker options are test-environment settings, not flags
added by rness to sandboxed commands.

The probe confirmed read-only write denial; workspace and private temporary
writes in workspace-write mode; outside-write, deletion, symlink-escape and
child-process write denial; and workspaces under `/tmp` containing spaces.
An unsandboxed control confirmed the test UID could otherwise write the fixtures.
This validates the backend command behavior on that kernel, not a Linux build or
end-to-end execution of the Rust application, nor every distribution's security
policy.
