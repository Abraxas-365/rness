# rness — Architecture

The founding document. Everything here was decided before the first line of
real code; deviations require a note in `docs/decisions/`.

**Pitch in one line:** an independent Rust coding agent with a plugin
kernel, traceable sessions, a Lua extension API, and a terminal UI.

**Positioning:** the Neovim of coding agents. A hackable agent where config,
tools, hooks, UI components, and plugins are Lua — and the user has the same
power as the vendor.

---

## 1. Why this project exists

rness is designed around Lua-first customization (`init.lua` and the
`rness.*` API), a terminal UI, skills, subagents, hooks, and multiple
providers. Its architectural goals are:

- A single retained Lua VM rather than configuration/runtime replay.
- Protocol boundaries between the TUI and engine.
- Append-only session history with reconstructable model input.
- Explicit composition through services and extension contracts.

These goals inform the two guiding principles:

- **Everything is a plugin** — extension points use shared contracts.
- **Every run is traceable** — model-visible input belongs in the durable log.

### Why Rust (decision record)

Rust was selected for:

1. **mlua** — real Lua 5.4/LuaJIT, async Lua functions awaiting Rust
   futures, compile-time send/sync guarantees. The entire dual-VM hazard
   class becomes unrepresentable. This is the single strongest technical
   argument; the Lua story IS the product.
2. **Open-source positioning** — "Rust + embedded Lua, the Neovim of coding
   agents" is the narrative; the analogy lands harder when the architecture
   mirrors Neovim (native core, Lua runtime). Note: Rust is NOT empty space
   (Goose, Codex CLI) — the differentiator is Lua-first hackability, not the
   language.
3. Fresh repo, no baggage, protocol-first from day 1.

Accepted costs: explicit ownership and concurrency design, and the build
complexity of a native Rust application with embedded Lua.

---

## 2. The kernel (rness-kernel)

Hand-rolled Cordis equivalent (~500–1000 lines). **No dependency on a DI or
plugin framework crate** — this is the project's identity. **Zero domain
knowledge**: if it mentions "session" or "LLM", it's in the wrong crate.

Four building blocks (straight from Cordis):

1. **Context** — the shared world plugins mount into; scopable.
2. **Services** — named slots (`ctx.tools`, `ctx.llm`, `ctx.sessions`). One
   plugin provides; consumers declare `inject: [...]`. A plugin does not run
   until its injected services exist and is stopped if one goes away. Boot
   order is derived from the dependency graph — nobody writes init order.
3. **Typed events** — dispatch modes are part of each event's contract:
   `emit` (fire-and-forget), `bail` (any listener vetoes), `waterfall`
   (middleware with `next()` — policy plugins wrap/short-circuit any
   decision), `serial`.
4. **Effects/disposers** — every side effect (service registration,
   listener, mount, timer) returns a disposer. Unload = run disposers =
   everything unwound. This is what makes hot reload safe by construction.

The agent loop, providers, tools, and session log are all plugins mounted
into this kernel. Lua plugins register into the **same seams** as Rust
plugins — peers, not second-class scripts.

---

## 3. The session log (rness-engine/session)

### Event sourcing

The session is an **append-only JSONL event log** — the source of truth.
The turn loop derives model requests by replaying the log; UI views are
projections. Consequences: replay, fork, resume, migration, multi-client
convergence, and golden-file testing all come from one mechanism.

**Core invariant (runtime-asserted): MODEL-VISIBLE MEANS LOGGED.** Anything
that reaches a model request must be reconstructable from the log. No
hidden context, ever.

**Attempt preservation:** failed / cancelled / retried attempts are kept as
events (deepseek's `assistant/attempt`) without polluting model history.
Committed messages embed the exact timed chunk stream that produced them.

**Versioned from day 1:** `session.v1.jsonl`. One migration module per
vN→vN+1 step. Committed generations are never mutated (copy-on-write).
Old fixtures are kept forever and migrations are tested against them.

### Branching

Branches preserve alternate conversation paths through log references:

- A branch is a **fork point in the log**: fork = new log + parent
  reference `{parent, branch_point}`. Prefix events are replayed from the
  parent, never copied. Parents are never mutated.
- The "leaf pointer" becomes a **projection** (current active path), not
  mutable state.
- Edit-and-resend, retry, and subagent trees are all this one mechanism.
- Engine API: `fork(session, at_event)`, `ancestry(session)`,
  `children(session, at?)` — exposed through SessionService so ANY frontend
  can branch.

**Rule of thumb:** if it changes what's *true* about a session (lineage,
events) → engine core. If it changes what you *see* (tree view, sibling
markers) → plugin.

### Storage: JSONL, not SQLite

JSONL is the source of truth. Reasons: append-only by nature (immutability
is physics, not discipline), human-readable/greppable (debugging is `less
session.jsonl`; "every run is traceable" rings truer when users can open
the file), git-committable test fixtures (the snapshot-replay test strategy
depends on this), trivial copy-on-write generations, zero deps.

SQLite is allowed **later** as a derived, disposable index only (session
listing, FTS5 full-text search, token rollups). Hard rule: **the index can
always be deleted and rebuilt from the logs.** If losing the .db loses
data, the design is wrong. rness initially ships with no SQLite at all.

---

## 4. The turn loop (rness-engine/turn, inbox, tools)

- **Turn flow is a documented contract:** `turn/start → pre-step → request
  → stream → tool pipeline → step/end`, with typed events at each stage;
  `waterfall` listeners can wrap or veto any of them (policy plugins).
- **One inbox, three intents:** user input while the agent runs is
  classified `followup` / `steer` / `inject`, with explicit phases
  (idle / running / maintenance), rather than ad-hoc queued messages.
- **Parallel tools, model-order commits:** bounded concurrent execution,
  deterministic history order.
- **Durable events vs live frames:** committed history and streaming
  display are explicitly separate types (`protocol/events.rs` vs
  `protocol/frames.rs`). Frames are never persisted.
- **Interaction contracts are UI-agnostic** (`interaction.rs`): approval,
  ask-user, commands are engine services; the TUI is one provider answering
  them, headless/server answer differently. (deepseek's `interaction`
  package lesson.)
- **Capability seams:** tools consume `ctx.fs` / `ctx.subprocess` services.
  Point them at a remote sandbox and Bash, PTY, and the file tree all move
  with it — no forks.

---

## 5. The Lua system (rness-lua)

The Lua design favors explicit ownership and shared service boundaries:

- **One retained mlua VM** using real Lua 5.4.
- **Async integration** through the VM actor and Rust service adapters.
- **Capability injection per context** for mounted engine services.
- Lua calls the **same services** as other extensions, not engine internals.
- Startup declarations are validated before constructing providers.

The `rness.*` API covers tools, hooks, keymaps, UI, branches, sessions,
filesystem access, HTTP, and events.

`examples/` holds EXAMPLE Lua in the repo — copyable references, not
embedded, not auto-loaded. Zero magic: rness loads only `~/.rness`.
(We tried the Neovim embed-defaults pattern and rejected it: a default
you didn't install is magic.)

---

## 6. The TUI (rness-tui)

deepseek has **no TUI** (web-first; ~40 `ui-*` browser plugin packages).
A first-class terminal experience is our differentiator — but their
ui-feature-as-plugin pattern is right, so we apply it *inside one crate*:

- **Core owns:** frame loop, focus, event routing, slot layout, and the
  non-swappable widgets (editor, message viewport, markdown/syntax
  renderer — themable and hookable, not replaceable day 1).
- **Slot registry:** named mount points — `statusline`, `sidebar`,
  `message_body`, `input_footer`, `overlay`, `picker`. Features mount
  `Component`s with priority + disposer.
- **Built-in modules** (chat, tool cards, approval overlay, pickers,
  statusline, branch markers) register through the same seam **Lua
  components** use → vendor and user have equal power; UI features
  hot-reload.
- **Perf guardrail:** a slow (Lua) component cannot block the frame loop —
  render budgets, cached buffers, async refresh.
- **Protocol only:** the TUI depends on `rness-protocol`, never engine
  internals. Web or nvim frontends later are additive.
- Packaging: **one crate, modules inside.** Crate-per-widget is compile-time
  and orphan-rule pain for zero benefit; the seam matters, not the package
  boundary.

### Apps ship as Lua (dogfood rule)

File tree, branch tree view, session picker live in `examples/plugins/*.lua`
as copyable examples (copy into `~/.rness/plugins/`, then explicitly select
with `rness.plugins.load("name")` in `init.lua`; see [plugin lifecycle](guides/plugins/loading-and-lifecycle.md)):

1. **Dogfood proof** — if a stateful, interactive, event-driven app like the
   file tree can be pure Lua, the plugin API is real; gaps surface before
   external authors hit them.
2. **The demo** — "our file tree is ~300 lines of Lua, fork it" is the
   strongest pitch to the Neovim crowd.
3. **Netrw lesson** — design built-ins for replacement from day 1.

Hybrid allowed where perf demands (Rust does fs walking/git-status as a
service; Lua owns behavior).

---

## 7. Workspace layout

```
rness/
├── Cargo.toml                    # workspace + workspace.dependencies
├── rust-toolchain.toml
├── crates/
│   ├── rness-kernel/           # context, services, events, effects, plugin
│   ├── rness-protocol/         # wire types ONLY: events, frames, branch, api
│   ├── rness-engine/           # session/ (log, branch, projection, replay,
│   │                             #   migrations/), turn/, inbox, tools/,
│   │                             #   interaction, invariants, service
│   ├── rness-providers/        # anthropic, openai, ollama, sse (bedrock later)
│   ├── rness-tools/            # read, edit, write, bash, grep, glob
│   ├── rness-lua/              # runtime, plugin_host, reload, api/ (rness.*)
│   ├── rness-tui/              # app, slots, component, core/, modules/, theme
│   ├── rness-server/           # axum HTTP/SSE over SessionService
│   └── rness-cli/              # the binary; sole composition root
├── examples/                     # example Lua plugins (copyable)
│   ├── init.lua
│   └── plugins/                  # tree.lua, branches.lua, sessions.lua
├── docs/
│   ├── architecture.md           # this file
│   ├── invariants.md
│   └── decisions/                # ADRs
└── tests/                        # cross-crate integration + snapshot replay
```

**Dependency graph (enforced by Cargo):**

```
kernel ◀── engine ◀── providers, tools, lua
   ▲          ▲
protocol ◀────┼──── server, tui     (frontends see ONLY protocol + service)
              │
             cli   (composition root — the only crate that knows everything)
```

Rules:

1. `kernel` knows nothing about AI.
2. `protocol` is serde-only; no tokio, no logic.
3. `cli` is the only composition root; it assembles the plugin tree from
   config layers (defaults → user → project → `--patch` overlay) and
   supports `--dump-config` (inspect the composed tree without booting).
4. Tools and providers are leaf plugins; deleting their crates must still
   compile the engine.
5. 9 crates; resist splitting further until compile times force it. Never
   crate-per-tool or crate-per-widget (deepseek's 80-package tax).

---

## 8. Crate choices

| Area | Crates | Notes |
|---|---|---|
| Runtime | tokio (full), tokio-util (CancellationToken) | |
| Kernel | *(none — hand-rolled)* | No DI/actor frameworks |
| Lua | **mlua** (lua54, vendored, async, serialize, send) | The heart |
| Providers | reqwest (rustls), eventsource-stream, serde_json, tiktoken-rs | Own the provider trait; NO LLM abstraction crates (they lag providers) |
| Session log | serde_json + tokio::fs, fd-lock, ulid, jiff | No DB |
| TUI | ratatui, crossterm, pulldown-cmark (own renderer), syntect, unicode-width, textwrap | tui-textarea as editor base, expect to fork; ratatui-image later |
| Tools | portable-pty, ignore, globset, grep-* (ripgrep internals), similar, notify | Grep tool = ripgrep-as-a-library |
| Server | axum (built-in SSE) | ONE wire protocol (dsh's RPC+SDK+ACP sprawl is a lesson) |
| CLI | clap, dirs, keyring (OS keychain for API keys), include_dir | |
| Errors/obs | thiserror (libs) + anyhow (edges), tracing stack | Spans map to turn/step/tool |
| Testing | insta + ratatui TestBackend, wiremock, proptest, tempfile | FTS via SQLite FTS5 later if needed (not tantivy) |

---

## 9. Testing strategy

Layered, stolen from deepseek's playbook:

1. **Unit/integration per crate** — every crate ships tests (their
   package-owns-its-tests rule).
2. **Recorded-session snapshot replay** (the crown jewel): committed
   session JSONL is both replay input and expected output. Every test
   process boots through the real `rness` binary — no hidden test
   entrypoints. Fake provider (wiremock) feeds recorded chunk streams.
   Sessions are normalization fixed points (volatile IDs → stable tokens).
   Workspace-mutating scenarios commit an independent oracle
   (`workspace.expected/` file tree) — model prose does not prove the
   external effect. Old `session.vN.jsonl` generations are kept forever;
   migrations are tested against them.
3. **TUI snapshots** — insta + ratatui `TestBackend`.
4. **Property tests** — proptest on log invariants ("model-visible means
   logged", append-only, branch lineage acyclic).
5. **Meta-verification in CI** — invariant assertions per crate, generated
   catalogs (tools/events/config) checked fresh, doc-sync gates.

---

## 10. Feature map

### From deepseek-harness (architecture DNA)
plugin kernel · event-sourced log · model-visible-means-logged ·
attempt preservation · inbox (followup/steer/inject) · turn flow contract ·
parallel tools with model-order commits · durable-vs-live split · config as
plugin tree + layered patches + `--dump-config` · capability seams
(fs/subprocess) · per-crate invariants · ADRs/postmortems in-repo ·
generated catalogs

### Product surface
Lua-first hackability + Lua API surface (as `rness.*`) · skills · custom
subagents (background runs, worktree isolation) · hooks (now sugar
over kernel waterfall events) · provider breadth · rich TUI (vim editor,
markdown, themes, pickers) · tool suite semantics (file freshness,
unique-match edits, ignore-aware search) · SessionService + server ·
MCP client · LSP/DAP · branch DAG (new substrate) ·
Project instruction files · ToolSearch · notify

### Integration goals
Lua plugins as kernel peers · single async mlua VM · protocol-first TUI
(multi-client) · session branching/replay UX actually shippable ·
`--dump-config` · apps-as-Lua (tree, branches, sessions)

### Explicitly NOT doing
deepseek's 80-package explosion · multiple wire protocols · Cordis as a
dependency · web UI (initially) · dual VM · TUI-engine entanglement ·
mutable session state · SQLite as source of truth · LLM abstraction crates

---

## 11. Milestones

Build order follows the dependency graph:

- **M0 — kernel**: context, services (+derived boot order), typed events
  (4 dispatch modes), disposers, hot reload. Exit: a toy app of 3 plugins
  with inject deps, waterfall interception, and clean unload.
- **M1 — protocol + engine**: event types v1, JSONL log, branching
  (fork/ancestry/path), projections, turn loop, inbox, tool dispatch,
  interaction contracts, invariant assertions, SessionService. Exit:
  headless one-shot turn against a fake provider, fully replayable;
  snapshot-replay harness running.
- **M2 — providers + tools**: anthropic + openai + ollama; the seven
  built-in tools on fs/subprocess seams. Exit: real end-to-end coding turn
  headless.
- **M3 — TUI core**: frame loop, slots, component trait, editor, viewport,
  markdown renderer, chat + tool cards + approval + statusline modules.
  Exit: daily-drivable TUI over the protocol.
- **M4 — Lua**: mlua runtime, plugin host, hot reload, rness.* API (tools,
  hooks, ui, branch, session, fs, http, events). No sandbox — plugins run
  with the user's permissions and the full stdlib (the Neovim trust
  model); rness.* is for integration, not confinement. Exit: a Lua
  plugin registers a tool, a hook, and a statusline component.
- **M5 — apps in Lua**: tree.lua, branches.lua, sessions.lua against the
  stabilized API. Exit: the dogfood proof.
- **M6 — server + ecosystem**: axum SSE server, MCP client, skills,
  completion of the remaining extension surface. Subagents SHIPPED (dsh model): the
  `subagent` seam in the engine (provider registry, spawn = fresh child,
  fork = seeded with the parent's completed-turn prefix, depth enforced
  from durable Delegation stamps in session headers), the model-facing
  `subagent` tool (foreground result / background as an ordinary job),
  and `rness.subagents` in Lua. A child is a NORMAL session — it shows
  up in pickers, lineage queries, and the log like any other.
- **Compaction SHIPPED** (dsh checkpoint model): `SessionService::compact`
  folds everything before a kept tail into a model-written summary and
  commits a `compaction/summary` event whose `replaces` list shadows the
  folded span. The log stays append-only — projections skip shadowed
  events and replay the summary at the fold's anchor; re-compaction
  shadows earlier checkpoints transitively. Surfaced as
  `rness.session.compact`/`usage` in Lua (autocompact.lua example),
  `POST /api/sessions/:id/compact` on the server, and a
  `history_changed` frame + transcript notice in clients. Layer below
  it: `prune_tool_results` (dsh tool-result-pruner) — deterministic,
  model-free head+tail cut of oversized tool results via
  `compaction/prune` events; `rness.session.prune` in Lua,
  `POST /api/sessions/:id/prune`. autocompact.lua runs prune first
  (free), compact when that is not enough.
- **Remote approvals SHIPPED**: under `--approval ask` in `--serve`
  mode the server's `RemoteApprovals` is the engine's answerer — a
  paused sensitive call surfaces as an ephemeral `approval_requested`
  frame over SSE, late clients reconcile the still-pending set from
  `GET /api/approvals`, and any HTTP client answers via
  `POST /api/approvals/:call` (one-shot; a second answer is 404).
  Cancelling the turn withdraws the question (`approval_resolved`
  frame, fail-closed as Cancelled) — `ToolRegistry::dispatch` takes the
  turn's CancellationToken so a pending approval can never outlive its
  turn. `ApprovalRequest` now carries the session id (protocol-owned,
  same shape everywhere).
- **Workspace instructions SHIPPED** (dsh agent-instructions model):
  `engine/instructions.rs` discovers instruction files (AGENTS.md
  chain) from the project root (nearest `.git`) down to the cwd,
  broad→specific, first candidate per directory wins, byte budget
  drops broad files whole before truncating the most-specific. The
  baseline is injected as a DURABLE user-role message (`UserMessage`
  gains `source: Option<MessageSource>`, `kind: instructions` with a
  sha1 identity over discovery inputs+content) — it replays, forks and
  compacts like any history. Before each turn `ensure_instructions`
  checks the PROJECTED surface for a visible baseline with the current
  identity; absent (first turn, compaction folded it, file changed on
  disk) → re-read disk, append fresh. Config is explicit
  (`InstructionsConfig { cwd, candidates, max_bytes }`, zero engine
  defaults); the CLI seeds it visibly (`--instructions
  AGENTS.md,CLAUDE.md`, `--instructions-bytes 65536`, `none`
  disables).
- **Request config SHIPPED** (dsh request/header model): per-session
  request controls live in the log as `request/config` events —
  durable, latest wins, only real changes are appended
  (`set_config` no-ops on equality; no silent per-call drift). The
  projection carries the effective `CallConfig` to providers inside
  `ModelContext`; each adapter maps `reasoning` to its own wire
  spelling verbatim: anthropic wants a thinking token budget
  (`"8192"` → `thinking.budget_tokens`, non-numeric fails before
  I/O), openai-compatible sends `reasoning_effort`, responses sends
  `reasoning.effort`. Absent = the knob is not sent — the provider's
  own default behavior, nothing invented. Seeds: `--reasoning`
  (explicit choice → logged event; omitted → touch nothing, resume
  keeps the logged choice) and `rness.session.config(id[, cfg])` in
  Lua.
- **User-declared routes SHIPPED**: `--route
  <name>=<url>[,<credential>|,none]` (repeatable) inserts an
  OpenAI-compatible route into the table before selection — new
  gateways (OpenRouter, Together, vLLM, LAN servers) without
  recompiling; a spec may replace a builtin name. Credential defaults
  to the route name (`<NAME>_API_KEY` env or the store), `none` =
  unauthenticated. Model ids keep their slashes (`-m
  openrouter/anthropic/claude-x`: first segment = route, rest =
  model verbatim). Other wire kinds stay code, not configuration
  (gateways are overwhelmingly OpenAI-compatible). Capabilities stay
  separate in `rness.models` (Lua): route = how to reach, capability
  = what the model can do — the registry key includes the route, so
  the same model via different gateways declares different facts.

Branching data model lands in M1 even though its UI waits for M5 —
retrofitting lineage into a log format is the mistake to avoid.
