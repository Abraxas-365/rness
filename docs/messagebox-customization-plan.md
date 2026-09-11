# Messagebox customization and tool presentation backlog

Status: implemented and verified for the scope below. Historical backlog notes are retained; documented memory and height-index limitations are not promises of constant-memory or constant-time rendering. Updated 2026-09-10.

## Latest verification

- Pre-commit: workspace library/integration tests and project doc-tests passed separately. Vendored Crossterm doc-tests are not part of that success claim. Both Unix parser backends preserve Escape followed by another escape-prefixed key; terminal QA verifies the coalesced PageDown sequence without filtering literal text.
- Visual row allocations are now bounded by `cache_bytes` (default 8 MiB), with eviction/regeneration and fingerprints replacing cached Entry clones. Tiny-budget terminal QA covered streaming, scrolling, resize, session changes and compaction. Transient rendering allocations, other caches and history/index storage remain outside this budget.

- Final acceptance follow-up: all supported configuration sections have a schema regression matrix (roles, Markdown, padding, borders, labels, markers, keys, tools and all four states). Fixed startup rejecting `tool.visible` despite the renderer supporting it. Existing rendering, precedence, legacy/structured/malformed-card and execution-budget regressions pass.
- Final `cargo test --workspace --quiet` including doc-tests passed; live Anthropic remains explicitly ignored. Output: `/tmp/rness-close-tests.log`.
- Rebuilt CLI: terminal `close-compact` reported `folded 2 events`, displayed the durable replacement notice, and correctly wrapped it after resize from 60×16 to 40×12. Isolated HOME/session fixtures only.
- These checks close the configuration/renderer acceptance follow-up and latest-binary compaction check; they do not establish a bounded total transcript cache or constant-time suffix-height adjustment.

- Workspace unit/integration tests passed. The command timed out during doc-tests; those passed separately. The live Anthropic test remains explicitly ignored.
- Cursor deltas avoid cloning/projecting the old TUI history. JSONL readers index event IDs and resume after complete lines.
- Append-only visual caching now visits only new entries: the regression adds one entry after 1,000 cached entries and asserts one visit. Compaction and width/theme/view/card changes retain conservative full-cache checking.
- Terminal `final-messagebox-qa`: 300 fragments, stable scroll across incoming deltas, fork during streaming, return with background deltas, resize and commit.
- Terminal `final-compact-qa`: Lua compaction reported `folded 2 events`; the transcript displayed the replacement notice.
- Headerless structured cards and multiline multi-span body rows now decode correctly, covered by a regression test.

- Full `cargo test --workspace --quiet` subsequently passed, including doc-tests; one live Anthropic test remains ignored.
- Write now captures separated changed ranges within the snapshot budget, with a regression test. Approval-cancelled metadata selects the cancelled tool style; failed turns select the error role.

Acceptance update: terminal-state visibility/display/style matrix is covered, as are legacy/structured/malformed renderers. Tool callbacks have a 100 ms Lua-instruction deadline and 16 MiB incremental VM allocation budget; these are not a sandbox or a deadline for blocking native functions. Card publication and tool/thinking view changes select affected entries by index; height-changing updates still adjust the suffix height index. Compaction, theme/width changes, and journal overflow deliberately rebuild. The transcript cache remains proportional to retained history; bounded total transcript memory is not implemented. Historical progress below is not a completion claim.

Durable metadata slice: ToolResult.presentation optional JSON added with serde omission/default; Tool.execute_presented defaults to existing rich execution, dispatcher retains metadata only when serialized size <=256 KiB (oversize currently silently omitted, needs diagnostic). Edit captures before/after/path at execution, bounded to 48 KiB source bytes; Write captures creation/overwrite and before/after within same bound, reading old files only up to a preliminary 24 KiB metadata size check (race-hardening pending). Larger snapshots explicitly truncated. Existing model-visible output unchanged. Updated all test literals. Workspace test compilation passes; tools/protocol/engine suites, immutable Edit/Write snapshot test and CLI build pass. Remaining: Lua renderer propagation, Read/Bash/Glob/Grep/jobs/skills/plugin metadata, durable reopen/provider-exclusion tests, better bounded hunks for large files, identity edge cases and full tool/diff terminal QA. TP tasks not complete.

Regression slice: added expansion header anchoring tests at 24/60/100 columns and a simple live-thinking -> first thinking part in committed assistant transition. All pass, plus full Lua/TUI suites and CLI build. These tests do not establish identity correctness when notices/intervening events or reordered/multiple thinking parts occur. No durable metadata field was left half-wired; protocol remains unchanged by this slice. Metadata implementation and full tool/diff terminal QA still pending.

Current slice: thinking expansion state now stored per (entry index, part index), retained when navigating blocks, and live thinking participates in focus/display. This is UI-lifetime state only; live-to-durable identity under intervening events still needs auditing. Tool toggles measure the updated layout at last actual area before calculating the scroll offset, avoiding stale total-row counts. This uses an extra render pass and needs dedicated viewport/performance tests. Nine Chat tests, CLI build and diff check pass. No durable tool metadata implemented yet; full tool/diff terminal QA still pending.

Latest validation: diff context_lines grouping and summary counts implemented/tested. Added previous_thinking/next_thinking key options (defaults alt+[/alt+]) and visible historical thinking focus marker; expansion state still needs persistence across focused blocks and live support. Real terminal QA launched in claudio:messagebox-qa using /tmp/rness-messagebox-qa-20260910/.rness/init.lua and localhost:1 only. Wide and 60x24 ANSI evidence: /tmp/rness-messagebox-qa-20260910/{wide,narrow}.ansi. Confirmed backgrounds/padding/border; discovered mid-word wrapping and expected clipped scrollback after resize. Tool/diff/live interaction QA not yet executed. Metadata and post-expansion anchor work still pending. Full Lua/TUI suite, CLI build and focused diff context test pass.

Progress: MB-03 style-resolution foundation implemented in theme.rs: all named theme groups, direct color/modifier overrides with field inheritance, explicit modifier removal, user_message group, and legacy card fallback. MB-02 now captures startup messagebox data separately from callbacks, validates option structure, and supports nested tool_card/replace_tool_card registration. User exact > user wildcard > plugin exact > plugin wildcard; style-only entries preserve plugin renderers. Startup assignments preserve nested methods; post-startup table replacement is rejected. Callback references survive via the Lua VM, separately from serialized config. Three messagebox tests pass; full Lua suite passed before the final config-validation addition, and CLI build passes afterward. Still pending: color/group validation against active themes, complete contract/bounds, structured card returns, actual TUI wiring/layout, metadata and remaining backlog. Neither MB-02 nor MB-03 is marked fully complete.
Repository: /Users/abraxas/Personal/rness.

Additional progress: CLI now broadcasts messagebox configuration to Chat. Opt-in conversation padding/border/background and role padding, first-line/bar markers, labels, full-width colors and borders render. Live assistant text uses the same panel helper. Tool panels support base/per-tool/terminal-state style, header name/status, output visibility and static collapsed/preview/expanded defaults. Existing no-config path retained. Full Lua/TUI suite and CLI build passed before the last tool-panel slice; focused Chat suite passed afterward. Remaining known gaps: interactive focus/expansion, structured cards, tool args/duration, wrap=false, accurate visual-row preview, grapheme-aware efficient wrapping (current helper is character/span based), empty role panels, complete live/replay equivalence, Markdown overrides, style validation, all tool metadata and terminal QA. Do not mark MB-04/05 complete yet.
These are local task IDs, not Linear issues. No cloud issues created.

Latest slice: configurable previous_tool/next_tool/toggle_tool keys and per-call expanded state added. Focus marker shown; session switches clear view state. Modal/sidebar focus blocks messagebox handling and editor retains first refusal. CLI rejects invalid style colors/groups and key chords. Empty assistant tool-only panels no longer insert role spacing. Lua/TUI suites and CLI build passed. Remaining interaction gaps: navigation does not yet scroll focused card into view, viewport anchor preservation, thinking navigation, complete key collision validation. Structured cards, metadata, diff blocks and real terminal QA still pending.

## Goal and constraints

Latest implementation: code/diff blocks now travel from Lua through StyledLine.block and render with line offsets, syntax spans, sign/number gutters and full-width added/removed backgrounds. Focused test checks offsets and background. Navigation computes a scroll action from cached card row positions (still needs post-expansion anchor correction). Thinking toggle currently targets the latest historical thinking block only; this is an interim implementation, NOT the agreed fully focused interaction. Lua/TUI suites passed, followed by diff test and CLI build. Durable tool metadata, diff context grouping/summary/bounds, live-thinking interaction and real terminal QA are still pending.

Structured-card slice: kernel StyledLine now carries optional styled spans and right-side spans; Lua accepts {header={left=...,right=...},body=...}, direct style tables, and span arrays. Existing line arrays preserved. TUI displays mixed header styles/right status and body styles. Lua/TUI tests, focused structured-card test and CLI build passed before a final multiline-body normalization change. Missing: structured diff/code blocks, robust span validation/bounds, multiline multi-span handling, inner-width-aware header alignment. Full metadata backlog remains pending.

Markdown slice: configurable heading/link/quote/inline-code styles, code-block style and horizontal padding, language-label toggle, and syntax-highlighting toggle are wired into configured assistant rendering. Added regression for heading/link colors and plain-code text preservation. TUI suite (78 tests at that point), subsequent focused Markdown test, CLI build and diff check pass. Still needs complete inherited live/replay consistency and vertical code padding; MB-06 not complete.

Give users and plugins extensive Lua-controlled conversation presentation, including screenshot-style compact Bash cards and syntax-highlighted Edit diffs. Rust supplies execution facts, layout, highlighting, and scrolling; Lua supplies appearance, renderers, and defaults.

- Do not use subagents/workers for implementation unless the user changes that instruction.
- Existing image work is uncommitted: preserve it. Do not include .claudio-session in commits. No push or personal-config changes authorized.
- Presentation never changes tool semantics, model-visible results, or durable message text.
- Persist bounded tool presentation facts, not terminal styles or rendered lines. Never reread a modified file to reconstruct a historical diff.
- Keep old text-only histories readable; absent metadata uses existing fallback rendering.
- No arbitrary user/assistant layout callbacks in this scope. Tool callbacks remain supported.
- All messagebox interaction keys configurable in rness.ui.messagebox.keys, independently of promptbox.
- Defaults preserve current appearance. New screenshots/styles are opt-in.
- Full means all retained output, not output discarded by execution limits. Show explicit truncation indicators.

## Current grounding

- `crates/rness-lua/src/api/config.rs`: startup promptbox declaration is serialized to JSON; functions cannot pass through that path.
- `crates/rness-lua/src/runtime.rs`: tool_card / replace_tool_card callbacks registered on ui, declaration-phase guarded.
- `crates/rness-lua/src/plugin_host.rs`: Lua tool-card boundary.
- `crates/rness-engine/src/presentation.rs`: engine presentation seam (inspect before editing).
- `crates/rness-tui/src/theme.rs`: fixed named style groups, fg/bg and bold/italic/underline/reverse; card_style currently supports a subset.
- `crates/rness-tools/src/lib.rs`: built-in inventory includes Read, Write, Edit, Bash, Glob, Grep, jobs, skills, subagent, subagent_control.
- Bash foreground captures stdout/stderr separately; background job output is combined. Do not invent stream provenance for combined output.
- `crates/rness-protocol/src/events.rs`: durable ToolResult with rich text/image content.

## Proposed API contract to finalize in MB-01

```lua
rness.ui.messagebox = {
  style = { bg = "#282828" },
  padding = { left = 1, right = 1, top = 0, bottom = 0 },
  spacing = 1,
  border = { kind = "none", style = { fg = "#504945" } },
  message = { padding = { left = 1, right = 1, top = 1, bottom = 1 } },
  user = {
    style = { fg = "#ebdbb2", bg = "#3c3836" },
    marker = { text = ">", mode = "first_line", style = "user_prefix" },
    label = { text = "You", style = "user_prefix" },
  },
  assistant = {
    style = "assistant_text",
    marker = false,
    label = false,
    markdown = {
      heading = "heading", link = { fg = "#83a598", underline = true },
      quote = "dim", inline_code = "code",
      code_block = {
        style = { bg = "#1d2021" }, padding = { left = 1, right = 1 },
        show_language = true, syntax_highlight = true,
      },
    },
  },
  thinking = { visible = true, display = "collapsed", style = "thinking" },
  tool = {
    style = "tool_output", spacing = 1,
    padding = { left = 1, right = 1, top = 0, bottom = 0 },
    border = { kind = "rounded", style = "dim" },
    header = { visible = true, show_name = true, show_status = true,
               show_duration = true, style = "tool_name" },
    arguments = { visible = true, style = "dim" },
    output = { visible = true, style = "tool_output", wrap = true },
    display = "preview", preview_lines = 8,
    states = {
      running = { border = { style = { fg = "#fabd2f" } } },
      success = { border = { style = { fg = "#b8bb26" } } },
      error = { display = "expanded", border = { style = "error" } },
    },
  },
  tools = {
    Read = { output = { wrap = false }, preview_lines = 20 },
    Bash = {
      display = "collapsed",
      render = function(call)
        return {
          header = {
            left = {
              { text = "Bash", style = "title" },
              { text = "  $ " .. (call.args.command or ""), style = "dim" },
            },
            right = { { text = call.status or "", style = "dim" } },
          },
          body = { { text = call.output or "", style = "tool_output" } },
        }
      end,
    },
    plugin_tool = function(call) return nil end,
  },
  notice = { style = "dim", marker = { text = "-", mode = "first_line" } },
  error = { style = "error", marker = { text = "!", mode = "first_line" } },
  keys = {
    next_tool = "alt+down", previous_tool = "alt+up",
    toggle_tool = "ctrl+o", toggle_thinking = "alt+t",
  },
}
```

Proposed keys above are examples, not finalized defaults. Audit collisions before choosing defaults.

- Styles accept active-theme group strings OR direct fg/bg/modifier tables. `default` means terminal default. Do not introduce fake palette names such as `surface` without explicitly defining them.
- Border kinds: none/plain/rounded/double. Marker modes: first_line/bar; false disables marker/label. Padding uses terminal cells/rows, nonnegative bounded integers.
- `display = collapsed|preview|expanded` supersedes earlier collapsed booleans and max_lines aliases. Collapsed shows header, preview clips retained body to preview_lines visual rows, expanded shows all retained body via conversation scrolling.
- Header uses left/right span arrays. Plain lines and styled spans are supported; custom structured cards own their header/body, built-in header is suppressed. Outer framing remains UI-owned.
- Simple legacy line-array callback returns remain whole-card rendering. Structured cards are preferred for expansion; finalize legacy expansion behavior explicitly in MB-01.
- `nil`, callback errors, or malformed renderer output fall back safely, with diagnostics for errors.
- Plugins can register defaults through messagebox.tool_card / replace_tool_card without replacing the configuration table. Startup table assignment must not erase registration methods.
- User renderer overrides beat plugin defaults independently of plugin load order. Style-only user overrides must not erase plugin renderers. Exact names and '*' fallback precedence must be explicit.
- Diff block: `{kind='diff', language=..., before=..., after=..., old_start=..., new_start=..., context_lines=3, line_numbers=true, summary=true, styles={added=...,removed=...,gutter=...}}`. Before/after refer to captured data, not live file reads.

## Execution order

Foundation: MB-01 -> MB-02, TP-01. UI and tool metadata can then proceed independently. Integration follows both. Each task has a checkable result and should be split further if its patch exceeds roughly 400-800 lines.

### MB-01 — Finalize configuration and renderer contracts [High]
**As a** plugin author, **I want** an unambiguous schema **so that** configuration composes predictably.
**Context:** Earlier examples evolved; this task resolves contradictions before implementation.
**Technical scope:** This document; inspect `crates/rness-lua/src/api/config.rs`, `runtime.rs`, `crates/rness-engine/src/presentation.rs`, `crates/rness-tui/src/theme.rs`. Estimated change: ~150 lines of contract/examples.
**What to do:** Specify schema, bounds, defaults, inheritance, state override precedence, exact/wildcard renderer lookup, callback input, structured blocks, and legacy line-card expansion. Define same-layer duplicate registration errors and explicit replacement. Define focused thinking navigation and tool wrap=false horizontal overflow behavior. Separate startup declaration from runtime interaction state; do not silently promise runtime config replacement.
**Acceptance:** [ ] Every example uses one canonical field name. [ ] User exact/wildcard vs plugin exact/wildcard precedence is documented with cases. [ ] Numeric/string limits, metadata size cap, and truncation policy have concrete values before dependent implementation starts. [ ] No unresolved renderer/config assignment collision.
**Out of scope:** Code implementation, personal configuration migration.
**Verification:** Review examples against current runtime/config seams; `git diff --check`.
**Dependencies:** None. Blocks all following schema work.

### MB-02 — Parse messagebox declarations and preserve callbacks [High]
**As a** user, **I want** one Lua table for settings/functions **so that** customization is cohesive.
**Technical scope:** `crates/rness-lua/src/api/config.rs`, `runtime.rs`, `plugin_host.rs`; follow promptbox validation and declaration phases. ~350 lines; split callback registry if needed.
**What to do:** Parse data separately from functions; store callbacks with Lua lifetimes; support nested plugin registration and user-over-plugin precedence. Preserve style-only renderer inheritance.
**Acceptance:** [ ] Table assignment retains plugin registration API. [ ] Reversing plugin load order does not change user precedence. [ ] Exact and catch-all cases pass. [ ] Unknown fields/type errors fail startup with field paths. [ ] Renderer nil/error fallback tested.
**Out of scope:** TUI layout and tool metadata.
**Verification:** `cargo test -p rness-lua`; `cargo build -p rness-cli`.
**Dependencies:** MB-01. Blocks MB-03/04/05/06.

### MB-03 — Theme references and direct style overrides [High]
**As a** user, **I want** theme-aware or fixed colors **so that** Gruvbox panels are customizable.
**Technical scope:** `crates/rness-tui/src/theme.rs`, Lua validation from MB-02. ~250 lines.
**What to do:** Resolve all supported style groups; add user_message and any approved missing groups; preserve card aliases such as title. Implement direct hex/named/default colors and modifiers. Define inherited field merging without losing explicit false modifiers.
**Acceptance:** [ ] Switching theme updates references but not hex overrides. [ ] Invalid groups/config colors rejected. [ ] fg-only override retains inherited bg. [ ] bold=false removes inherited bold.
**Out of scope:** Palette API, personal Gruvbox edits.
**Verification:** `cargo test -p rness-tui`; `cargo test -p rness-lua`.
**Dependencies:** MB-01/02.

### MB-04 — Conversation and role message framing [High]
**As a** user, **I want** colored, padded messages **so that** roles are visually distinct.
**Technical scope:** `crates/rness-tui/src/modules/chat.rs`, `core/render.rs`, `app.rs`, CLI config wiring in `crates/rness-cli/src/main.rs`. ~400 lines; split wiring if necessary.
**What to do:** Conversation style/padding/border; shared message and per-role overrides; first-line/bar markers, labels, spacing; notice/error style. Fill full panel width including padding, retaining outer spacing background.
**Acceptance:** [ ] Wrapped text aligns after gutter. [ ] Full-width background and borders snapshot-tested. [ ] Unicode markers and narrow widths do not overflow/panic. [ ] Streaming and equivalent replay match. [ ] Existing defaults unchanged.
**Out of scope:** Tool expansion, arbitrary message callbacks.
**Verification:** `cargo test -p rness-tui`; `cargo build -p rness-cli`.
**Dependencies:** MB-02/03.

### MB-05 — Structured tool cards and default card options [High]
**As a** plugin author, **I want** styled headers/bodies **so that** compact screenshot-style cards are possible.
**Technical scope:** `crates/rness-lua/src/runtime.rs`, `plugin_host.rs`, `crates/rness-engine/src/presentation.rs`, `crates/rness-tui/src/modules/chat.rs`. ~400 lines; separate wire representation from layout if needed.
**What to do:** Decode string/styled-line/span content and header left/right sections; apply tool defaults/per-tool/state styles, padding, borders, header toggles, args/output visibility and wrapping. Preserve custom full-card ownership; no duplicate headers.
**Acceptance:** [ ] Bright tool name and dim command render on one line. [ ] Right status remains within width when command is long. [ ] Custom cards do not gain a built-in header. [ ] nil/error/malformed callbacks fall back. [ ] Existing line-array renderers still work.
**Out of scope:** Diff algorithm and focus/expansion.
**Verification:** `cargo test -p rness-lua`; `cargo test -p rness-tui`; `cargo test -p rness-engine`.
**Dependencies:** MB-02/03.

### MB-06 — Markdown and thinking presentation [Medium]
**As a** user, **I want** configurable rich text and reasoning blocks **so that** the transcript fits my preferences.
**Technical scope:** `crates/rness-tui/src/core/render.rs`, `core/highlight.rs`, `modules/chat.rs`. ~300 lines.
**What to do:** Heading/link/quote/inline-code/code-block styles, code padding/language labels/highlighting toggle; thinking visibility, labels, marker, display mode. Preserve nested span foregrounds while inheriting message bg unless explicitly overridden.
**Acceptance:** [ ] Markdown fixture covers every option. [ ] Highlight-off preserves exact code text. [ ] Hiding thinking leaves durable history/model context unchanged. [ ] Code and image blocks remain within message width.
**Out of scope:** New Markdown parser or syntax theme engine.
**Verification:** `cargo test -p rness-tui`.
**Dependencies:** MB-04. Interaction depends on MB-07.

### MB-07 — Focus, expansion, and viewport stability [High]
**As a** user, **I want** to expand individual tools/thinking **so that** compact output remains inspectable.
**Technical scope:** `crates/rness-tui/src/app.rs`, `modules/chat.rs`, inspect existing input/event routing; config key validation. ~400 lines; split focus model and viewport work if needed.
**What to do:** Stable call/block identity; configurable navigation/toggle keys, visible focus indicator; collapsed/preview/expanded modes; retain body data; preserve viewport anchor during expansion and streaming. Finalize thinking navigation from MB-01. View state is session-local UI state, not model events.
**Acceptance:** [ ] Two identical tool names expand independently. [ ] Hidden body reappears intact up to capture cap. [ ] Resize/streaming does not toggle another card. [ ] Disabled/remapped keys work and do not consume prompt editing keys unexpectedly. [ ] Narrow and long-output cases scroll without overflow.
**Out of scope:** Mouse support and durable UI preference history unless separately approved.
**Verification:** `cargo test -p rness-tui`; real terminal QA in MB-11.
**Dependencies:** MB-05/06.

### TP-01 — Durable presentation metadata and lifecycle seam [High]
**As a** renderer author, **I want** reliable execution facts **so that** cards do not parse formatted text.
**Technical scope:** `crates/rness-protocol/src/events.rs`, `crates/rness-engine/src/tools/mod.rs`, session projection/service, provider serializers/tests. ~400 lines; split protocol plumbing from lifecycle if needed.
**What to do:** Optional versioned, bounded JSON presentation metadata through rich execution/result persistence; extension namespacing and invalid/oversize policy per MB-01. Expose call ID/status/duration from existing execution events; distinguish available terminal states honestly. Exclude presentation metadata from model projection.
**Acceptance:** [ ] Old JSONL replays. [ ] Metadata survives reopen and HTTP event roundtrip. [ ] Provider payloads with/without metadata are identical. [ ] Invalid plugin metadata cannot crash session or turn success into a hidden failure. [ ] Execution failure/cancellation lifecycle tested.
**Out of scope:** Terminal layout and changing tool-visible output.
**Verification:** `cargo test -p rness-protocol -p rness-engine -p rness-providers -p rness-server`.
**Dependencies:** MB-01. Blocks TP-02 through TP-11.

### TP-02 — Edit metadata [High]
**As a** user, **I want** accurate historical edit diffs **so that** I can inspect changes after the file changes again.
**Technical scope:** `crates/rness-tools/src/edit.rs`, existing Edit tests; follow freshness and unique-match semantics. ~250 lines.
**What to do:** Capture bounded before/after hunks, context, resolved path, old/new offsets and counts during the successful write. Handle replace-all hunks and newline edge cases.
**Acceptance:** [ ] Multiple hunks have accurate original/new coordinates. [ ] Editing file again does not change prior card data. [ ] Failed/stale edit never reports applied changes. [ ] Large diff signals capture truncation.
**Out of scope:** Changing matching/freshness semantics, rereading for presentation.
**Verification:** `cargo test -p rness-tools`.
**Dependencies:** TP-01.

### TP-03 — Write metadata [High]
**As a** user, **I want** creation/overwrite summaries **so that** Write cards reflect what happened.
**Technical scope:** `crates/rness-tools/src/write.rs`, existing Write tests. ~200 lines.
**What to do:** Capture created/overwritten, resolved path and bounded before/after changes using execution-time data.
**Acceptance:** [ ] New file is creation. [ ] Overwrite retains before snapshot and accurate counts. [ ] Empty files/no final newline tested. [ ] Failed write emits no success diff.
**Out of scope:** New backup/undo feature or weakening freshness checks.
**Verification:** `cargo test -p rness-tools`.
**Dependencies:** TP-01.

### TP-04 — Read metadata [High]
**As a** user, **I want** numbered code previews **so that** Read output is easy to navigate.
**Technical scope:** `crates/rness-tools/src/read.rs`. ~150 lines.
**What to do:** Capture path, language hint, actual start line, returned raw text and truncation; reuse existing image references when applicable, without duplicating image bytes.
**Acceptance:** [ ] Offset/limit produces correct gutter. [ ] Binary/image/text cases remain distinct. [ ] No uncaptured file content appears. [ ] Model-visible output unchanged.
**Out of scope:** New file formats or reading beyond requested limits.
**Verification:** `cargo test -p rness-tools`.
**Dependencies:** TP-01.

### TP-05 — Bash metadata [High]
**As a** user, **I want** command/status/output cards **so that** shell results are understandable without string parsing.
**Technical scope:** `crates/rness-tools/src/bash.rs`, job integration. ~250 lines.
**What to do:** Capture command, actual cwd, exit/signal/timeout status, bounded stdout/stderr where separately available, background job reference. Preserve combined model result. Never claim total cross-stream ordering.
**Acceptance:** [ ] Success/nonzero/timeout/cancellation distinguishable. [ ] Foreground streams preserved separately where captured. [ ] Background combined stream honestly labelled. [ ] No environment/credential dump added.
**Out of scope:** PTY emulation and changing shell execution semantics.
**Verification:** `cargo test -p rness-tools`.
**Dependencies:** TP-01.

### TP-06 — Glob metadata [Medium]
**As a** user, **I want** structured path results **so that** search cards need not parse display strings.
**Technical scope:** `crates/rness-tools/src/glob.rs`. ~120 lines.
**What to do:** Retain query/root, matched paths, returned count and known truncation status within existing limits.
**Acceptance:** [ ] Empty/multiple/truncated results match existing output ordering. [ ] No extra filesystem traversal. [ ] No invented total count when unknown.
**Out of scope:** New glob semantics or sorting changes.
**Verification:** `cargo test -p rness-tools`.
**Dependencies:** TP-01.

### TP-07 — Grep metadata [Medium]
**As a** user, **I want** structured matches **so that** paths, line numbers and text can be styled separately.
**Technical scope:** `crates/rness-tools/src/grep.rs`. ~200 lines.
**What to do:** Preserve output mode, paths, available line numbers/context/match text, returned counts and truncation; capture structure before formatting rather than parse text afterward.
**Acceptance:** [ ] Content/files/count modes each tested. [ ] Missing line numbers remain absent, not guessed. [ ] Unicode paths/text preserved. [ ] Search scope and limits unchanged.
**Out of scope:** Search engine replacement.
**Verification:** `cargo test -p rness-tools`.
**Dependencies:** TP-01.

### TP-08 — Jobs and subagent tool metadata [Medium]
**As a** user, **I want** accurate task status cards **so that** asynchronous work is inspectable.
**Technical scope:** `crates/rness-tools/src/jobs.rs`, `subagent.rs`, `subagent_control.rs`; inspect registrations for all exported tools before editing. ~300 lines, split by module if needed.
**What to do:** Add presentation facts for each existing action/tool: stable job/run ID, label, observed status, bounded output, cancellation result where available. Do not invoke subagents to implement this task.
**Acceptance:** [ ] Every registered action is covered by a test or documented plain fallback. [ ] Historical observations do not silently change with live job state. [ ] Failed cancel is not shown as cancelled. [ ] Unknown duration/status stays unknown.
**Out of scope:** New orchestration functionality or worker execution.
**Verification:** `cargo test -p rness-tools -p rness-engine`.
**Dependencies:** TP-01.

### TP-09 — Skill tool metadata [Medium]
**As a** user, **I want** a concise skill card **so that** loading a skill is recognizable.
**Technical scope:** `crates/rness-tools/src/skills.rs`. ~100 lines.
**What to do:** Capture skill name, source identifier/path if already available, loaded/error status, bounded display summary. Preserve returned instructions exactly.
**Acceptance:** [ ] Successful and missing skill cases tested. [ ] No duplicated full instruction document in metadata. [ ] No execution policy changes.
**Out of scope:** Skill discovery redesign.
**Verification:** `cargo test -p rness-tools`.
**Dependencies:** TP-01.

### TP-10 — Lua/plugin result metadata [High]
**As a** plugin author, **I want** optional presentation data **so that** my tools get the same card capabilities.
**Technical scope:** `crates/rness-lua/src/runtime.rs`, `plugin_host.rs`; locate tool execution bridge from `crates/rness-engine/src/tools/mod.rs`. ~250 lines.
**What to do:** Extend existing Lua result contract with optional bounded presentation data; pass it plus lifecycle facts to render callbacks; preserve old string/rich returns.
**Acceptance:** [ ] Plugin data persists and reaches custom renderer. [ ] Legacy returns unchanged. [ ] Unknown namespaced data does not break fallback. [ ] Oversize/malformed data follows TP-01 policy. [ ] User renderer/style precedence covered end-to-end.
**Out of scope:** Executable callbacks persisted in history.
**Verification:** `cargo test -p rness-lua -p rness-engine`.
**Dependencies:** TP-01, MB-02/05.

### TP-11 — MCP and image presentation integration [High]
**As a** user, **I want** external and image tools to remain first-class **so that** customization works beyond built-ins.
**Technical scope:** `crates/rness-mcp/src/lib.rs`, `crates/rness-engine/src/images.rs`, `crates/rness-tui/src/modules/chat.rs`, existing rich-result tests. ~250 lines.
**What to do:** Use existing ordered MCP text/image results and attachment metadata for cards. Pass only approved optional structured metadata when available; do not assume an MCP extension exists. Reuse durable image references and session authorization.
**Acceptance:** [ ] Text-image-text order retained. [ ] MCP errors still visible. [ ] No-metadata tools use default cards. [ ] Image dimensions/media type shown without duplicating encoded bytes. [ ] Expansion preserves image access controls.
**Out of scope:** New MCP protocol, remote URL fetching, image upload changes.
**Verification:** `cargo test -p rness-mcp -p rness-engine -p rness-tui`.
**Dependencies:** TP-01, MB-05/07.

### MB-08 — Structured code and diff block rendering [High]
**As a** user, **I want** screenshot-quality diffs **so that** precise changes are readable.
**Technical scope:** `crates/rness-tui/src/core/render.rs`, `core/highlight.rs`, `modules/chat.rs`, Lua block decoder. ~400 lines; split diff computation and layout if needed.
**What to do:** Code/diff blocks with separate line-number/sign gutters, context hunks, summary counts, syntax fg over full-width addition/removal bg, explicit gutter styles. Consume execution snapshots, never current filesystem contents.
**Acceptance:** [ ] Insertion/deletion/replacement/multiple hunks accurate. [ ] Added/deleted line numbers correct. [ ] Background fills width, not only code glyphs. [ ] Narrow widths, long lines, tabs, Unicode and missing final newline tested. [ ] Preview/expanded diff displays retained data and capture truncation distinctly.
**Out of scope:** Side-by-side diff, word-level diff, editable diff UI.
**Verification:** `cargo test -p rness-tui -p rness-lua`.
**Dependencies:** MB-05/07, TP-02/03/04.

### MB-09 — Lifecycle presentation wiring [High]
**As a** user, **I want** live status and duration **so that** cards reflect execution without waiting for completion.
**Technical scope:** `crates/rness-engine/src/service.rs`, `crates/rness-lua/src/plugin_host.rs`, `crates/rness-tui/src/app.rs`, `modules/chat.rs`; inspect existing event paths. ~250 lines.
**What to do:** Feed existing running/terminal call events and metadata to cards; run state override precedence; keep stable identity across updates; bound rerender work.
**Acceptance:** [ ] Running -> success/error/cancelled renders without duplicate cards. [ ] Duration uses actual recorded values. [ ] Historical replay has no fake running state. [ ] Collapsed state survives result arrival.
**Out of scope:** New execution scheduling and provider behavior.
**Verification:** `cargo test -p rness-engine -p rness-lua -p rness-tui`.
**Dependencies:** TP-01, MB-05/07.

### MB-10 — Migrate repository examples and document API [High]
**As a** user, **I want** copyable examples **so that** customization requires no hidden conventions.
**Technical scope:** `docs/guides/plugins/example-recipes.md`; locate repository diffcards.lua and theme examples with Glob; do not modify ~/.rness files. ~300 lines.
**What to do:** Migrate bundled registrations to messagebox, use structured cards and captured metadata, remove manual preview truncation, show theme/direct color styles and plugin overrides. Audit other bundled plugin registrations and decide old API migration according to MB-01.
**Acceptance:** [ ] All examples load under tests. [ ] Compact Bash and highlighted Edit/Write examples exist. [ ] Full retained body can expand. [ ] No example relies on ambient PWD for historical paths. [ ] Documentation clearly distinguishes view limits and capture limits.
**Out of scope:** Personal config migration or silent API aliases beyond approved contract.
**Verification:** `cargo test -p rness-lua`; `git diff --check`.
**Dependencies:** All MB feature tasks, TP-02 through TP-11.

### MB-11 — Regression, performance, and real terminal acceptance [High]
**As a** user, **I want** validated terminal behavior **so that** customization stays reliable while streaming.
**Technical scope:** Existing tests in `crates/rness-tui`, `rness-lua`, `rness-engine`, `rness-providers`, `rness-server`; isolated temporary QA config/evidence. ~300 lines of durable regression tests.
**What to do:** Test defaults and Gruvbox-style config; compact Bash, full-width message panels, Edit diff, plugin fallback, images, thinking, expansion, theme switching, resizing and long outputs. Use isolated local fixtures, no cloud calls required. Capture actual terminal evidence and record commands/results.
**Acceptance:** [ ] Workspace tests/build pass. [ ] Wire tests prove metadata exclusion from every provider. [ ] Real terminal narrow/wide captures show no overflow or duplicated headers. [ ] Focus and viewport remain stable during streaming. [ ] Large retained output does not render every offscreen row unnecessarily; record measured behavior before claiming performance. [ ] Existing promptbox/clipboard/history image behavior remains intact.
**Out of scope:** Committing/pushing or altering personal config without authorization.
**Verification:** `cargo test --workspace`; `cargo build -p rness-cli`; `git diff --check`; real tmux/terminal QA with saved evidence.
**Dependencies:** All other tasks.

## Deferred / not promised

Mouse navigation, arbitrary role-message render functions, side-by-side or word-level diffs, durable per-card view state, a new palette API, terminal image protocol changes, live config hot reload, and full unbounded output capture are outside this backlog.

## Completion checklist

- [ ] MB-01 through MB-11 complete with evidence.
- [ ] TP-01 through TP-11 complete with evidence.
- [ ] All built-in registrations audited, including job/subagent-control variants; any discovered tool not covered above gets its own scoped metadata task rather than being silently skipped.
- [ ] User approves any remaining contract choices before implementation depending on them.
- [ ] No UI example presented as implemented until its task passes verification.
