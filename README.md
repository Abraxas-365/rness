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

## Install from a checkout

With the repository's Rust toolchain installed:

```sh
./install.sh
```

This builds with `cargo build --locked --release`, installs `~/.local/bin/rness`,
and copies `flavors/default/` to `~/.rness` only if that directory does not exist.
Existing configuration is never overwritten or merged. To upgrade the binary,
run `./install.sh --replace-binary`; your configuration stays untouched.

The default flavor includes Gruvbox cards, session/branch/task/plan plugins,
manual compaction, and specialist agents. The scout uses `small.by_provider` in
`~/.rness/lua/providers.lua`: Anthropic Haiku is configured; add explicit mappings
for other provider connections before delegating to scout. No fallback is used.
Configure credentials separately, then select your principal explicitly:

```sh
rness --model anthropic/claude-haiku-4-5-20251001
```

Use `--binary /path/to/rness` to install an already-built executable without Cargo.
`--bin-dir` and `--config-dir` change installation destinations; the latter does
not change where rness looks for its runtime configuration (`$HOME/.rness`).
The installer does not use sudo, modify shell startup files, or download models.
Add `~/.local/bin` to your PATH if needed. Test installation safety offline with
`python3 scripts/test_install.py`.

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
