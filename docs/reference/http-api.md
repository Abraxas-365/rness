# HTTP API notes

`rness --serve ADDR` exposes the session service over HTTP/SSE. This page
covers behaviour that clients must handle; wire types live in
`crates/rness-protocol` (`api`, `frames`).

## Sending input: `POST /api/request`

`{"type":"send","session":ID,"intent":"followup","content":[…]}` answers
`{"status":"started"|"queued"|"logged"|"command"}`.

Concurrent sends to one session are serialized by the server (each waits up
to 5 seconds for the session's admission lock): the first to an idle session
starts the turn (`started`), the others are queued as followups (`queued`) and
run as one batched later turn. None is rejected merely because another send
is in flight.

A send that cannot be admitted — an extension reload or compaction is in
progress, or a slash command is running — answers **409 Conflict** with a
`Retry-After: 1` header and a plain-text `session is busy …` body. Retry
after the delay. A busy session is never reported as a 500.
