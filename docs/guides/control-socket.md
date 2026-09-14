# Experimental local submission API

This feature is **opt-in at build/install time and at runtime**. Normal installs
exclude both `--control-socket` and `rness send`; no listener starts implicitly.

```sh
./install.sh --experimental-control
# Existing executable replacement still requires explicit authorization:
./install.sh --experimental-control --replace-binary
# Development build:
cargo build -p rness-cli --features experimental-control
```

`bootstrap.sh` forwards `--experimental-control`. With `--binary FILE`, the
installer checks that the supplied binary's capability matches the explicit
opt-in. Existing configuration is never changed. Do not run the installer to
replace another installation unless you intend to upgrade that executable.

## Start and send (Unix)

```sh
private=$(mktemp -d)
chmod 700 "$private"
rness --control-socket "$private/control.sock" --profile YOUR_PROFILE
# From another shell/editor, use the session ID of the displayed conversation:
printf 'Investigate this failure\nDetails here' | rness send \
  --socket "$private/control.sock" --session SESSION_ID --id UNIQUE_REQUEST_ID
```

`rness send` accepts a positional text argument or reads UTF-8 stdin. `--id` is
required: reuse the same ID, session and text after an uncertain acknowledgment.
It initializes no provider/configuration and cannot answer dialogs. IDs must be
unique across the session root, not just a connection. Use a UUID or equivalent.

Unix sockets require an existing current-user-owned 0700 parent; the socket is
0600. Existing paths, including stale sockets, are never overwritten. Normal
shutdown removes the owned socket. After a crash, use a fresh socket path.

## Windows implementation (not validated on Windows yet)

The experimental build includes a native named-pipe implementation:

```powershell
cargo build -p rness-cli --features experimental-control
rness --control-socket '\\.\pipe\rness-editor-UNIQUE' --profile YOUR_PROFILE
rness send --socket '\\.\pipe\rness-editor-UNIQUE' --session SESSION_ID --id UNIQUE_REQUEST_ID 'Hello'
```

Both client and server reject remote/UNC pipe names. The server rejects remote
clients and uses a current-user-only DACL. The Bash installer is not a native
Windows installer; build through Cargo there. Windows code has not been compiled
or runtime-tested in this macOS workspace and is not advertised as production
support. The local Neovim client remains Unix-oriented.

## Delivery and recovery

Acknowledgment means the request is fsynced to
`<session-root>/control/submissions.jsonl`, not that a model turn finished.
Pending text survives shutdown/crash and replays when that session is displayed
in a control-enabled TUI. Switching sessions is supported: the handshake reports
the actual displayed session and a switch during admission rejects the request.
Hidden-session queues wait until that session is displayed. FIFO is per session.
One control-enabled process owns a session root at a time (advisory journal lock).

An idle-only dispatcher commits the submission ID with the user message in the
session log. Restart/lost-ack retries cannot append that prompt twice, including
the crash window between the user-message commit and journal completion marker.
This is **at-most-once prompt insertion**, not exactly-once model/tool execution.
A crash after the prompt commit but before the turn starts leaves the prompt in
history; review/retry that turn normally. Nothing automatically repeats tool side
effects. Busy prompts never answer plan/question/tool-approval dialogs.

Non-busy delivery failures retain the pending text, block that session's queue,
and show a native notice. Fix the configuration/workspace issue and restart the
TUI to retry. New submissions to a blocked session fail rather than piling up
silently. Existing-ID retries still return their original durable acceptance.

The journal retains IDs and payloads for duplicate checks. It is bounded at
128 MiB / 65,536 IDs with reserved completion space; new submissions fail near
capacity, while already accepted work can drain. There is **no automatic payload
compaction yet**. With the TUI stopped, archive the journal only after reviewing
pending work; archiving resets deduplication, so never retry old IDs afterward.
It contains sensitive prompt text. Neovim also keeps recovery copies accessible
through `:RnessRecover`.

## Wire v1

One newline-delimited UTF-8 JSON request per connection; handshake-only discovery
is allowed. The server sends:

```json
{"version":1,"session":"SESSION_ID","durable":true}
```

The client sends exactly:

```json
{"id":"UNIQUE_REQUEST_ID","session":"SESSION_ID","text":"Prompt\nDetails"}
```

Reply:

```json
{"id":"UNIQUE_REQUEST_ID","session":"SESSION_ID","status":"accepted","durable":true}
```

Failures return `{"error":"description"}`. Wrong sessions, unknown fields,
empty text, slash-prefixed input, IDs over 128 bytes and requests over 1 MiB are
rejected. Slash commands must be typed in the native prompt. No separate history,
settings, cancellation, steering, approval, or session-management operations are
exposed. Same-user processes are trusted: submitted prompts may trigger tools
under the existing session permissions. Server reads/writes have three-second
timeouts; CLI response and write waits are bounded.

## Validation

```sh
cargo test -p rness-cli --features experimental-control
cargo test -p rness-engine --test service external_delivery
cargo test -p rness-tui displayed_session_observer
cargo check -p rness-cli --no-default-features
cargo build -p rness-cli --features experimental-control
python3 scripts/test-control-socket.py
python3 scripts/test_install.py
```

The Unix smoke test uses a real TUI/PTY and local scripted provider, not a real
model: CLI stdin, multiline text, durable queued replay after process kill,
duplicate IDs, slash rejection and visible response. Unit tests cover torn tails,
journal ownership, pending questions/plans and session switching. Windows needs
its own compilation/runtime validation before release.
