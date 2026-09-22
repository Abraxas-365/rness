# Provider configuration reference

A provider declaration names a connection: wire protocol, endpoint, and authentication. It does not select a model.

```lua
rness.providers.register("router", {
  protocol = "openai-chat",
  base_url = "https://openrouter.ai/api/v1",
  auth = { env = "OPENROUTER_API_KEY" },
  headers = {
    ["HTTP-Referer"] = "https://my-app.example",
    ["X-Title"] = "My App",
  },
})
```

## Fields

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| Name argument | string | Yes | Nonempty connection name; cannot contain `/` |
| `protocol` | string | Yes | `openai-chat`, `anthropic`, or `chatgpt-responses` |
| `base_url` | string | Yes | HTTP(S) endpoint base appropriate to the adapter |
| `auth` | boolean or table | Yes | Explicit authentication strategy below |
| `headers` | table of string names to string values | No | Additional provider request headers; defaults to `{}` |

Duplicate startup provider names fail. Unknown top-level declaration fields fail. User declarations can replace built-in connection names when the CLI assembles its route table.

## Custom headers

`headers` applies to every model on the connection, across all three protocols.
It is sent on inference requests (including retries) and supported file upload,
metadata lookup, and deletion requests. It is **not** sent to OAuth login or
refresh endpoints. File caches and quota cleanup are isolated by header values,
so different tenant headers do not share upstream file IDs.

Names must be valid HTTP header names; they are case-insensitive, and declarations
such as `X-Tenant` plus `x-tenant` are rejected. Values must be strings of at most
8192 bytes without control characters (including tabs and newlines). Invalid or
reserved headers fail CLI startup, even for connections not currently selected.

Authentication, transport, and adapter-managed headers are reserved:

- `authorization`, `proxy-authorization`, `x-api-key`, `cookie`, `host`
- `content-length`, `content-type`, `content-encoding`, `transfer-encoding`,
  `connection`, `keep-alive`, `te`, `trailer`, `upgrade`, `expect`
- `accept`, `accept-encoding`, `user-agent`
- `anthropic-version`, `anthropic-beta`, `anthropic-dangerous-direct-browser-access`, `x-app`
- `openai-beta`, `originator`, `chatgpt-account-id`

Continue using `auth` for built-in authentication. Treat custom values as secrets:
header diagnostics redact values, but literals remain in your configuration file.
Use HTTPS for sensitive values. With custom headers configured, only same-origin
redirects are followed (same scheme, host, and effective port), preventing their
forwarding to a different server or an HTTP downgrade.

## Authentication

Choose one strategy:

| Declaration | Resolution |
| --- | --- |
| `auth = false` | No authentication; supported for `openai-chat` only |
| `auth = { env = "VARIABLE" }` | Read exactly that nonempty environment variable as an API key |
| `auth = { credential = "entry" }` | Read the API key from exactly that credential-store entry |
| `auth = { oauth = "entry" }` | Use stored OAuth tokens and the adapter's refresh behavior |

Explicit environment and stored-key strategies do not silently fall back to each other. `auth = true` is invalid. OAuth is supported by the Anthropic and ChatGPT Responses adapters, not the generic OpenAI chat adapter. ChatGPT Responses requires OAuth rather than an API key.

Example stored-key management:

```sh
rness auth set-key --provider router
rness auth status
```

With no key positional argument, `set-key` reads standard input. Avoid placing keys directly in shell command arguments, where history or process inspection may expose them. Browser login is available through `rness auth login --provider anthropic` and `rness auth login --provider openai-chatgpt`.

Custom OAuth store references select an entry; they do not define an arbitrary OAuth issuer or login implementation.

## Multiple accounts per provider

A single provider connection can hold more than one stored credential — for example two Anthropic accounts. Credentials are addressed by key in `credentials.json`: the bare entry name (`anthropic`) is the **default account**; a named account uses `entry/account` (`anthropic/work`).

This applies to the `credential` and `oauth` auth strategies. `env`-based auth reads a single environment variable and has no account concept; switching accounts on an `env` connection means changing the variable's value yourself.

### Storing keys for named accounts

```sh
rness auth set-key --provider anthropic sk-ant-default-key
rness auth set-key --provider anthropic --account work sk-ant-work-key
rness auth login --provider anthropic --account personal
```

`--account` is accepted by `set-key`, `login`, and `logout`. Omitting it addresses the default (bare) entry, unchanged from prior behavior. `rness auth status` lists every stored account per provider.

### Selecting an account

```sh
rness --account work -m anthropic/claude-sonnet-4.5 -p "hello"
```

`--account` is a top-level CLI flag, independent of `--model`/`--provider`. It applies to whichever connection the request selects; a connection without a matching stored credential for that account name fails with a clear error at request time, not at startup.

### Declaring a default account

```lua
rness.providers.register("anthropic", {
  protocol = "anthropic", base_url = "https://api.anthropic.com",
  auth = { credential = "anthropic" },
})
rness.providers.set_default_account("anthropic", "work")
```

`set_default_account` must follow `register` for the same connection name and only accepts a bare account name (no `/`). It is a startup-only declaration, like other `rness.providers.*` calls; changing it requires a restart.

### Resolution order

For each request, the account actually used is:

1. `--account NAME` on the command line, if supplied.
2. `default_account` from the connection's registration, if declared.
3. The default (bare) stored entry, if it has a credential.
4. The first account with a stored credential for that connection, in ascending name order (the default account, if any, still sorts first).

Step 4 means a provider connection with only named accounts and no bare entry still works without `--account` or `default_account` — rness uses whichever account was stored, rather than failing because the unnamed slot is empty. When no credential exists under any account, the error names the connection and points at `rness auth set-key`.

## Endpoint validation

URLs must parse with a host and use HTTP or HTTPS. Usernames, passwords, query strings, and fragments are rejected. Include required adapter prefixes such as `/v1` for an OpenAI-compatible endpoint. A syntactically valid URL does not prove protocol compatibility.

Prefer HTTPS for remote endpoints. Plain HTTP is useful for local development but does not protect transmitted credentials or prompts.

## Model IDs

A profile stores the connection and model ID separately:

```lua
rness.profiles.declare("router-sonnet", {
  provider = "router",
  model = "anthropic/claude-sonnet-4.5",
})
```

The model ID is opaque to selection parsing. This example must be replaced if your endpoint offers a different ID. In combined CLI syntax, only the first slash separates the connection:

```sh
rness --model router/anthropic/claude-sonnet-4.5
rness --provider router --model anthropic/claude-sonnet-4.5
```

These selections identify the same connection/model pair. The `router/` prefix is rness's connection namespace, not part of the upstream model ID.

## Legacy CLI routes

`--route 'name=url[,credential|,none]'` declares an OpenAI-compatible connection for that invocation. A CLI route overrides the same-name startup connection and removes its explicit startup auth strategy and custom headers. Prefer `init.lua` declarations for reusable configuration with unambiguous authentication.

## Stream inactivity watchdog

The default deadline is **five minutes (300000 ms)**, matching DSH's DeepSeek
adapter. Configure a connection in `~/.rness/init.lua`, then restart rness:

```lua
-- Five minutes without a stream event or complete SSE comment.
rness.providers.set_stream_idle_timeout("anthropic", 300000)
rness.providers.set_stream_idle_timeout("router", 60000)
-- Zero disables it; omitting the declaration keeps the five-minute default.
rness.providers.set_stream_idle_timeout("deepseek", 0)
```

The first argument is the connection name, not the model ID. It supports built-in,
Lua-registered and CLI-declared connections. Unknown names fail startup. The last
setting for a name wins; values must be integer milliseconds from 0 through
2147483647. This is a startup declaration, not a dynamically unloadable plugin
setting. It affects every model on the connection, including headless/server use.

### What resets the deadline?

All three HTTP adapters (Anthropic, OpenAI chat-compatible, ChatGPT Responses)
use an explicit iterator watchdog. Each pull has a fresh deadline; complete SSE
comment lines reset it while waiting. Fragmentary bytes do not count as progress.
Time spent processing a returned event is excluded. This is **not** a total turn
or generation duration limit: a healthy stream can run indefinitely. Keepalives
can keep it alive even without tokens. Waiting for initial HTTP response headers
also has the configured deadline.

Like DSH, this is not a timeout for AskUser, approval, or tool execution. OAuth
credential acquisition uses its own transport and is not covered by this setting.
Timeout failures are retryable; Ctrl+C remains cancellation. Partial failed output
is never committed as a successful assistant message.

## Failures and recovery

Each failed provider attempt remains in durable session history. The TUI shows
its model, attempt number (reset after a committed model response), and cause.
History updates are published after each failure, not only when the turn ends.
The current engine default allows two automatic retries (three attempts total)
per model step for retryable failures. Non-retryable errors, such as HTTP 401,
stop immediately. Retry waits use exponential backoff (500 ms, 1000 ms for the
default budget; capped at 32 seconds for larger budgets). HTTP `Retry-After`
seconds or GMT HTTP-date overrides that delay, capped at 24 hours; malformed
values fall back to backoff. Ctrl+C interrupts the wait. Each error persists a
`code` and optional `retry_in_ms`; the TUI shows the scheduled next attempt.
Older logs without these fields remain readable. A failed turn displays an
explicit recovery-stopped notice. `/help retry` and `/retry --help` describe
manual recovery, and `/retry` is included in TUI completion.

Enter **`/retry`** in the TUI to start a new turn from the saved context. The command
is local UI control, never a prompt sent to the model. It does not append another
user message, and existing committed assistant/tool-result messages remain in
context. Previously committed tool calls are not replayed by the engine; this is
not an exactly-once guarantee against a model choosing to request a new tool call.

Retry is accepted only when the session is idle and its final durable event,
ignoring request-configuration changes, is a failed turn. This allows switching
models before retrying. Busy, empty, successful, cancelled, or otherwise modified
sessions are rejected. It is not queued and cannot race another session mutation through
the operation reservation. Use Ctrl+C to cancel an active retry normally.

HTTP clients can POST the same operation to `/api/request`:

```json
{"type":"retry","session":"SESSION_ID"}
```

A successful admission returns `{"status":"started"}`; ordinary lifecycle/history
SSE frames then report progress. Timeout configuration belongs to the local
connection and is not stored in the session log; reopening uses current startup
configuration.

## Local fault-injection QA

`scripts/fake_provider.py` is a standard-library Python Anthropic SSE simulator.
It binds only to localhost and requires no real credentials:

```sh
python3 scripts/fake_provider.py --port 8769 --stall-seconds 30
# In another terminal, use an isolated HOME if you have real startup credentials.
ANTHROPIC_API_KEY=fake-local-only rness -m anthropic/fake \
  --base-url http://127.0.0.1:8769/stall --instructions none
```

Configure a short timeout such as 500 ms in the isolated HOME's `.rness/init.lua`.
The URL's first path segment chooses the scenario:

| Scenario | Expected behavior |
| --- | --- |
| `ok` | Complete response |
| `cut` | Partial stream closes without terminal event; fails |
| `disconnect` | Connection closes before response headers |
| `stall` | Partial text then silence for `--stall-seconds` |
| `heartbeat` | Two seconds of SSE comments every 100 ms, then success |
| `recover` | First request returns 401; later requests succeed (server-wide counter); test `/retry` |
| `malformed` | Invalid JSON in SSE |
| `stream-error` | Explicit provider error event |
| `http-401`, `http-429`, `http-500` | HTTP errors; 401 stops, 429/500 retry |

Unknown scenario names currently return success. The simulator does not implement
model reasoning or tools. Restart the simulator to reset `recover`. Verify that
`stall` fails with a configured timeout, `heartbeat` survives a 500 ms timeout,
and `recover` succeeds after `/retry` without a duplicate user message. With the
timeout disabled, `stall` waits until cancellation or the server closes; closure
is an incomplete-response error, not an inactivity timeout.

Implementation: [startup declaration parser](../../../crates/rness-lua/src/api/config.rs), [CLI composition](../../../crates/rness-cli/src/main.rs), [provider routes](../../../crates/rness-providers/src/routes.rs).
