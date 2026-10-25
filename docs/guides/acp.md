# ACP: rness as an Agent Client Protocol agent

`rness --acp` runs rness as an [Agent Client Protocol](https://agentclientprotocol.com)
(ACP v1) agent. It reads JSON-RPC 2.0 from stdin and writes JSON-RPC to
stdout, one JSON message per line. Any ACP client can use it: an editor
(Zed, for example), a channel gateway that connects WhatsApp or Telegram to
coding agents, or another agent harness.

```sh
rness --acp --approval ask -m anthropic/claude-sonnet-4-5
```

Stdout carries only protocol messages. Logs go to stderr, or to the log file
when one is configured. The usual flags still apply (`-m`, `--approval`,
`--root`, `--reasoning`, …), and so does your `init.lua`: plugins, tools,
agents and MCP servers all load the same way they do for the TUI. When the
client closes stdin, rness cancels any running turns and exits with code 0.

Example Zed configuration:

```json
{
  "agent_servers": {
    "rness": { "command": "rness", "args": ["--acp", "--approval", "ask"] }
  }
}
```

## What is supported

| Method | Notes |
|---|---|
| `initialize` | Advertises `loadSession`, `sessionCapabilities.{list,resume,close}`, and `promptCapabilities.{image,embeddedContext}`. `image` is `false` when the startup model declares `image_input = false`. No authentication methods. |
| `session/new` | `cwd` must be an absolute path and becomes the session workspace. A non-empty `additionalDirectories` is rejected (`-32602`). |
| `session/load` | Replays the committed history as updates (your prompts, reasoning, text, tool calls and results), then returns. Refuses sessions already open on the connection or running, and subagent sessions (`session is not resumable`). |
| `session/resume` | Like `load`, without the replay. |
| `session/list` | Root sessions that are not open or running, newest first, optionally filtered by `cwd`. Pages of 100 with an opaque `nextCursor`. |
| `session/close` | Cancels any running prompt and forgets the session on this connection. The log is kept. |
| `session/prompt` | Accepts `text`, `resource_link` (flattened to `[name](uri)`), text `resource`, and `image` (PNG, JPEG, WebP or GIF as canonical base64; refused when the session's model declares `image_input = false`). Every block is checked before any image is stored. `audio` and blob resources are rejected. Only one prompt can be in flight per session; a second one gets `-32602`. Slash commands work and answer with a single `agent_message_chunk`. |
| `session/cancel` | Stops the running turn. The prompt then answers with `stopReason: "cancelled"`. |
| `session/set_config_option` | `model` (a `provider/model` value) or `reasoning_effort` (see below). Unknown options and values get `-32602`. |

The agent reports each session's `title` through `session_info_update`.

An unknown or unopened `sessionId` gets `-32602` (`unknown session: …`).

### Config options

`session/new`, `load`, `resume` and `set_config_option` return
`configOptions`:

- `model` (category `model`): a select grouped by provider. Each option's
  value is `provider/model`, from the configured model catalog.
- `reasoning_effort` (category `thought_level`): present only when the
  selected model declares `reasoning.efforts`. The `""` value ("Provider
  default") sends no explicit effort. Switching to a model that does not
  declare the current effort clears it.

When a slash command such as `/model` changes the configuration, the agent
sends `config_option_update` with the new options.

### Updates are committed facts

rness does not forward individual token deltas. After each model step and
each tool result is appended to the session log, it reads those events back
and sends them:

| Log event | ACP update |
|---|---|
| Assistant reasoning | `agent_thought_chunk` |
| Assistant text | `agent_message_chunk` (the `messageId` is the log event id) |
| Tool call | `tool_call` (`kind` mapped from the tool, `rawInput`, file `locations`) |
| Tool result | `tool_call_update` (`completed` or `failed`, text and image content) |
| Usage | `usage_update` (when the model declares a context window) |

As a result, live clients and replays (`session/load`) see the same
sequence, and a client never receives text the log does not contain.

Cancelling a prompt while tools run settles every pending call with a
`failed` `tool_call_update` (a running tool gets a short grace period to stop)
before the prompt answers `cancelled`.

### Approvals

When a sensitive tool needs approval (under `--approval ask`, or a tool rule
set to `ask`), the client receives `session/request_permission` with two
options: `allow-once` and `reject-once`. A `cancelled` outcome cancels the
call. An unknown option, an error, or a closed connection counts as a
rejection, so approvals fail closed and no answer is ever stored as a
permanent grant. Approvals from subagents go to the session the client
opened.

### MCP servers from the client

`session/new`, `session/load` and `session/resume` accept `mcpServers`. The
client is trusted: a stdio entry (`command`, `args`, `env`) starts its
command with the session folder as working directory and only the listed
environment on top of the usual base; an HTTP entry (`type: "http"`, `url`,
`headers`) connects to that URL. SSE is not supported.

- Everything is checked before anything starts. A bad entry fails with
  `-32602`: the command must be an absolute path, the URL must be `https`
  (plain `http` only on loopback) without credentials or fragment, and
  env/header entries must be valid and unique (header names ignore case). Server names that are not `[A-Za-z0-9_-]{1,32}` get a
  stable short name (a slug plus a hash), and two entries may not end up
  with the same one.
- All servers connect and list their tools before the session is returned.
  If one fails to start (internal error), the ones already started are
  stopped and no session is created or opened.
- Tools appear as `mcp__<server>__<tool>`. They belong to that session only:
  other sessions and its subagents do not see them, and a tool with the same
  name as a global one replaces it for that session. Approval rules apply by
  name as for any other tool. Like the MCP tools from `init.lua`, they do not
  ask for approval by default, and the server process runs outside the
  sandbox.
- `session/close` or closing the connection stops the servers. Nothing about
  them is saved in the session log: `session/load` and `session/resume`
  take the list again.
- Each MCP request has a 60 s timeout.

The MCP servers declared in `init.lua` keep working for every session.

## Not supported

- The client's filesystem and terminal (`fs/*`, `terminal/*`). rness always
  uses its own tools inside its sandbox.
- SSE MCP servers (`type: "sse"`).
- Modes, plans, `authenticate` methods, audio prompts.
- `ask_user` questions. There is no question overlay in ACP mode, so the
  tool is unavailable.

## Implementation

The protocol lives in the `rness-acp` crate (`crates/rness-acp`). Like
`rness-server`, it is a frontend over `SessionService`. Every change to
durable state goes through the service. The crate has no state of its own
beyond a per-connection cursor into each session's log. End-to-end check
against a real binary:

```sh
RNESS_E2E_BIN=target/debug/rness python3 scripts/e2e/acp_e2e.py
```
