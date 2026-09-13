# Known limitations

This page records observed boundaries in the current checkout. It is not a roadmap or a promise of a release date.

## Configuration and Lua

- Startup configuration requires restart. Explicit watched file/linked-package plugins support registration reload and automatic busy retry; managed packages and inline sources are not watchable. Unwatched source snapshots may execute again during registration rebuilds.
- Startup declarations close after `init.lua`; post-mount plugins cannot add providers, profiles, capabilities, or agents through those declaration methods.
- Lua module search retains existing `package.path` entries. Personal module paths are not a sandbox.
- Lua command cancellation and coroutine cleanup use cooperative instruction hooks, not hard time limits. Trusted plugin code can catch hook errors or block in native calls; a pathological callback or `__close` handler can still stall the VM.
- Engine APIs are unavailable at startup top level. The CLI synchronizes Lua tools before `ready`, but frontend session selection is not guaranteed.
- Reload rollback covers registrations, not arbitrary Lua/global/filesystem/network effects. Required global modules remain cached.
- Component defaults and legacy keymaps remain separate from scoped declarations. App-provided help cannot verify arbitrary handlers. Completion/paste-preview core mappings do not make all internal image controls independently scoped actions.
- Queued plugin actions and UI batches have conservative input/focus/generation guards; unrelated subsequent terminal input can discard slow actions. Full coverage of every focus/race interleaving has not been established.
- The 15-case terminal harness covers inline/file/linked-package activation and mappings, not a complete managed-Git end-to-end workflow. Package unit tests use local Git fixtures; they do not validate public transport/authentication.

## Messagebox presentation

- `cache_bytes` bounds retained visual-row allocations only, not total transcript memory, transient rendering, Markdown/card caches, or history indexes. Height-changing updates can adjust a suffix index; rendering is not universally constant-time.
- Width changes still reflow the durable transcript, so resize cost grows with history. Configured code-card previews now highlight only the source prefix needed for the preview (plus one truncation row); expanded cards and diffs retain full rendering. The ignored `diagnostic_streaming_resize_performance` test measures render-only costs, not end-to-end tmux latency.
- Tool/thinking expansion is session-local UI state, not a durable preference. Presentation metadata never changes model-visible output and must not be reconstructed by rereading current files.
- Lua card callbacks have instruction/allocation limits, not a sandbox or a timeout for blocking native calls. Missing, malformed, oversized, or truncated metadata must have a useful fallback.
- Messagebox schema/rendering regressions and prior terminal streaming/resize/compaction checks do not certify every possible plugin renderer or retained-history size.
- Legacy model-registry examples and process-global model exposure have not all been migrated to per-session configuration. Do not assume a statusline reports the active session's selected model or exact live context size.

## Context reduction

- Explicit startup `rness.compaction["provider/model"]` budgets enable pre-step pruning and summarization, including closed steps within a running turn. See `examples/init.lua`. No route is enabled implicitly.
- Measurement is heuristic, configurable per route via `meter = {bytes_per_token=4, message_tokens=8, image_tokens=4096, output_reserve=2048}`. Pressure reserves the larger of the configured reserve and request output cap. Retention and shrink checks use the same route meter. This is not an exact tokenizer or provider image-pricing implementation.
- Token-budget retention keeps the newest message and preserves assistant/tool-result boundaries. Pruning is remeasured immediately; empty or non-shrinking summaries are not committed. Original events remain in the log.
- Overflow recovery is bounded and requires a durable reduction. Recognized HTTP and streaming error envelopes are classified for OpenAI-compatible, Anthropic, and Responses adapters, including `response.failed`. Partial stream chunks remain failed-attempt data. Arbitrary gateway-specific variants are not inferred from loose message matching.
- Boundary summarization supports explicit `summary_selection = {route='...', model='...'}` per policy. The service resolves it before committing the opening prompt; unavailable routes fail rather than silently falling back. Without a selection the active provider is used.
- Boundary summaries append `compaction/started` and `compaction/finished` audit events, including source IDs, model, estimate, logical request (system, turns, config, tools), outcome, returned usage and chunks. Model-request JSON bodies are recorded as `compaction/request` before sending in the OpenAI-compatible, Anthropic, and Responses adapters; durable acknowledgement gates transmission. Headers, credentials, URLs, and auxiliary upload/auth traffic are excluded. These bodies can contain sensitive conversation data or inline images, just like the logical request. These events never enter model context. Resuming a send closes unmatched starts as interrupted or committed-before-interruption without repeating the request; unrecovered usage/chunks remain unavailable. Lifecycle and checkpoint writes are not atomic.
- Manual regions are available through `session.compaction_view(id)` and idle-only `session.compact_region(id, {start=..., end=..., sources=view.sources, policy=...})`. Indices are one-based inclusive model-message positions, not raw log events. The engine rejects stale source snapshots and split tool pairs; summaries retain their replaced span's position. The synchronous API still blocks the Lua VM during summarization. Command callbacks can instead call `session.compact_region_async(id, opts)` with the same options and boolean result: it suspends only the command coroutine while summarization runs on Tokio, leaving Lua frame hooks and status refreshes responsive. The command retains its session/extension reservation and cancellation token until completion; cancellation closes the suspended coroutine rather than continuing arbitrary command code. Service errors propagate back to its caller (provider failures retain the existing reducer behavior: a durable failure record and `false`, meaning history unchanged). This is not a detached job API and is not available from status callbacks or ordinary hooks. The default `/compact` and optional region-confirmation command use this asynchronous path; existing copied plugins must update their call to opt in. The optional `commands.lua` example offers `/compact-region`, `/compact-region start end`, `confirm`, and `cancel`, with truncated message previews, a 100-message selection limit, and an explicit paid-request warning. A Ctrl+G overlay supports j/k navigation, space anchoring, Enter range review, and r snapshot refresh. Confirmation still requires closing the overlay and running `/compact-region confirm`. Terminal testing against an isolated local mock verified navigation, range review, and successful confirmation with a persisted request body and committed checkpoint. Command callbacks carry an opaque, session-specific reservation permit so compaction retains the command's lock rather than reacquiring it; ordinary callers still require their own reservation. Command cancellation is passed to summarization. The widget's `key_help` metadata is restored; registration uses indexed reads to avoid mlua 0.10.5's under-reserved sequence-iterator stack, with regression coverage for the full example set and varying metadata lengths.
- This is not full DSH parity: exact tokenization and the complete lifecycle/recovery matrix remain outstanding. Captured JSON bytes are compared against a mock OpenAI HTTP request; equivalent byte-level adapter tests and exhaustive disk-failure injection remain outstanding. Existing idle `session.compact`/`session.prune` APIs retain their previous behavior.
- Do not also enable the legacy turn-end `autocompact.lua` policy for a route configured with boundary compaction.

## Agents

- `/agent <name>` is available through TUI and HTTP sends. There is no public `rness.agents.list()` Lua query.
- `rness.subagents.roster()` exposes delegable roles, not all principal roles or all child sessions.
- CLI principal snapshots do not share every validation step with service-based selection, including unknown-tool validation.
- Synchronous Lua delegation blocks the VM actor. Work requiring that same VM to execute callbacks may not be appropriate for this API.
- Allowed tool names are not a security sandbox. Trusted plugins and powerful tools can have broader effects than their names suggest.

## Providers and compatibility

- There is no complete maintained built-in model-capabilities catalog. Endpoint IDs and supported reasoning modes must be verified for your deployment.
- OpenAI-compatible describes a protocol family, not identical behavior across gateways.
- Local HTTP authentication tests verify adapter request behavior; they do not establish live OAuth entitlement or model availability.
- These pages target the source checkout. No release-level configuration or session-format compatibility guarantee is established by this documentation.

## Documentation coverage

The first documentation set covers installation, startup, profiles, agent roles, and delegation. Detailed reference for every Lua namespace, HTTP endpoint, event type, and built-in tool is still being written.

The original architecture document contains design intent alongside historical context. Consult current implementation and tests before treating an unreferenced architectural example as an available API.

When reporting a problem, include the revision, invocation, relevant redacted configuration, and observed error. Do not publish credentials or an entire session log without reviewing it for sensitive prompts, tool output, and workspace content.
