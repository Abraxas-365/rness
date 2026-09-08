# rness

**The Neovim of coding agents.** A terminal-first AI coding agent where the
config, tools, hooks, and UI components are Lua — and the user has the same
power as the vendor.

> Status: pre-alpha scaffold. The design is settled; the code is not.
> Read [docs/architecture.md](docs/architecture.md) first.

## Principles

- **Everything is a plugin.** No privileged core: the agent loop, model
  adapters, tools, and session log mount into a plugin kernel. Lua plugins
  register into the same seams as Rust plugins — peers, not scripts.
- **Every run is traceable.** Sessions are append-only JSONL event logs.
  Anything the model saw is reconstructable from the log — asserted at
  runtime. Branch, fork, replay, and resume fall out of one mechanism.
- **Hackable by default, zero magic.** rness loads only `~/.rness` — no
  embedded defaults. `examples/` holds copyable example plugins (the file
  tree is a Lua plugin on purpose); `rness --dump-config` shows exactly
  what composed.

## Layout

| Path | What |
|---|---|
| `crates/rness-kernel` | Plugin kernel: services, typed events, disposers |
| `crates/rness-protocol` | Wire types (the only thing frontends see) |
| `crates/rness-engine` | Event log, branching, turn loop, inbox, tools seam |
| `crates/rness-providers` | Anthropic, OpenAI, Ollama adapters |
| `crates/rness-tools` | Read/Edit/Write/Bash/Grep/Glob |
| `crates/rness-lua` | Single async mlua VM, `rness.*` API |
| `crates/rness-tui` | ratatui client: slots + components |
| `crates/rness-server` | HTTP/SSE over SessionService |
| `crates/rness-cli` | The binary; sole composition root |
| `examples/` | Copyable example Lua plugins (nothing auto-loads) |
| `docs/` | Architecture, invariants, ADRs |

## Design

rness is an independent Rust project combining a Lua extension API, a
terminal UI, skills, subagents, and hooks with a plugin kernel,
append-only sessions, and protocol-based frontend boundaries.

See [the documentation](docs/README.md) for configuration, usage, and
current limitations.
