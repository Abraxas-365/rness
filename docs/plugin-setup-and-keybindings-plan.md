# Explicit plugin setup and scoped keybindings

Status: implementation and regression-tested increment ready for commit; original acceptance checklist not exhaustively certified.

Validation completed: workspace all-target tests, doctests, Clippy (warnings remain),
optimized CLI build, and 15 isolated real-PTY cases against the release binary.
`scripts/plugin_acceptance.py` covers inline/file/linked-package activation, default
bindings, remapping, disabling, and Lua-startup core submit/no-op mappings plus help.
Terminal cases use a temporary HOME and do not submit provider requests. Unit and
integration regressions additionally cover reload/unload, busy retry, atomic file
replacement, focus invalidation, modal precedence, action batches, and named-action
collisions. These results are not a claim that every checklist item below was tested
end-to-end.

## Implemented API and remaining gaps

Explicit setup supports `file`, installed `package`, and inline `config` sources.
Setup callbacks receive `(opts, plugin)`. Relative files resolve against the configuration directory.
Installation does not activate plugins. Restart after changing startup declarations.

```lua
rness.plugins.setup({
  {
    name = 'review',
    file = './review.lua',
    watch = true,
    keys = { inspect = '<F8>' },
  },
})
rness.keymap.setup({
  { scope = 'global', key = '<F4>', action = 'core.scroll_up_page' },
})
```

An entrypoint can return this setup function:

```lua
return function(opts, plugin)
  plugin.action('inspect', {
    scope = 'messagebox',
    description = 'Insert a review prompt',
    run = function(ctx)
      ctx.promptbox.insert('Review this conversation')
    end,
  })
  plugin.keys({ inspect = { action = 'inspect', key = '<F6>' } })
end
```

Binding slots accept a string, a dense list of chords, or `false` through setup overrides.
`keys = false` disables all plugin defaults. Central mappings are additive and validated
against registered action scopes. They are startup-only. Actions use `plugin-name.action-id`.
App actions use scope `app:app-name`; their context provides `ctx.app.close()`.
Operations are buffered until a callback succeeds, and context methods expire afterward.
Messagebox bindings are considered only after the prompt declines the key.
App bindings apply only to the focused external app, without global fallback.

Reload preserves working registrations on validation failure. Busy reloads retry without
another save. Session-unloaded plugins are excluded from subsequent reload selections.
Registration generations reject stale callbacks and results; app activation generations
protect close/reopen cycles. Lua plugins remain trusted code, not sandboxed transactions.

Core prompt/messagebox actions support scoped central mappings. Map a chord to
`core.promptbox.noop`, `core.messagebox.noop`, or `core.app.noop` to disable it;
map another chord to the desired action to relocate it. App close uses
`core.app.close` with scope `app:<name>`. Completion and paste-preview actions use
`core.promptbox.completion_*`, `core.promptbox.preview_*`, and
`core.promptbox.close_preview`; internal modes only accept applicable core mappings.
Prompt/messagebox named actions execute by identity, not by replaying another key.
Help accounts for scoped overrides of declared component controls.

Remaining certification work: exhaustive checklist-to-evidence reconciliation,
managed Git package end-to-end workflow, all focus/race combinations, and complete
mode/app help coverage. The original phases/checklists below are the target
specification, not a record that every item has passed.

## Goal

Make plugin sources, execution timing, configuration, and keyboard behavior explicit. Retain the existing package installer and runtime ownership machinery. Borrow Neovim's separation of actions and mappings and Lazy's declarative setup, without implicit source discovery or key-triggered loading.

Installing a package must not activate it, execute plugin Lua, edit personal configuration, or change bindings.

## Constraints

- Do not edit `~/.rness` or migrate personal configuration without permission.
- No automatic installation, directory scanning, or file/package fallback.
- Preserve declaration order and defer runtime execution until required APIs are mounted.
- Keep plugins trusted code; registration rollback is not a sandbox or rollback of arbitrary side effects.
- Keep keybindings configurable and disableable across core UI and plugin UI.
- Rust supplies input dispatch, UI operations, and registration mechanisms; Lua supplies plugin behavior and presentation policy.
- No commits or pushes implied by this plan.

## Current integration points

Verify these paths and their current behavior before implementation:

- `crates/rness-lua/src/api/config.rs`: startup declarations and configuration validation.
- `crates/rness-lua/src/loader.rs`: local-file and package resolution.
- `crates/rness-lua/src/packages.rs`: install/link/update/remove and package snapshots.
- `crates/rness-lua/src/runtime.rs`: Lua execution, ownership, staging, reload, unload.
- `crates/rness-lua/src/plugin_host.rs`: actor commands and runtime/UI synchronization.
- `crates/rness-lua/src/reload.rs`: watcher roots and captured selection.
- `crates/rness-cli/src/main.rs`: deferred loading, readiness, installer CLI.
- `crates/rness-tui`: input routing, promptbox/messagebox configuration, panel handlers and help labels.
- `examples/plugins` and `docs/guides/plugins`: examples, lifecycle and package documentation.

Audit findings to verify and address:

1. Bare names can resolve as packages or local files.
2. Watcher classification is tied to the configuration root and its `plugins` subtree; external linked packages are not adequately covered.
3. Reload uses a captured selection that can reactivate a session-unloaded plugin.
4. Package-removal CLI messaging disagrees with managed checkout deletion.
5. Readiness is emitted before all registration synchronization completes.
6. Current registration rollback cannot undo filesystem/network effects or arbitrary Lua state mutations.
7. Existing host bindings, component-specific key settings, and raw panel key handlers must be inventoried before introducing a unified registry.

## Target configuration contract

```lua
rness.plugins.setup({
  {
    name = "branches",
    file = "./plugins/branches.lua",
    watch = true,
  },
  {
    package = "review-helper",
    opts = { instruction = "Review this code for bugs." },
    keys = {
      insert_review = "<F8>",
      quote_message = false,
    },
  },
  {
    name = "personal-ui",
    config = function(opts, plugin)
      -- Deferred runtime customization with plugin ownership.
    end,
  },
})
```

Rules:

- `setup` records one ordered list; it does not execute plugins immediately.
- Exactly one of `file`, `package`, or inline `config` is required.
- `name` is required for files and inline callbacks. Packages use their manifest identity; no aliases initially.
- Resolve relative file paths against the directory containing `init.lua`, never the process working directory.
- Do not implicitly expand `~`; users may supply an absolute path using `os.getenv("HOME")`.
- `enabled` defaults to true. False skips source resolution and execution, but the declaration must still be structurally valid.
- `watch` defaults to false and is supported for file sources and linked development packages. Reject it for inline callbacks and managed immutable installations with an actionable message.
- `opts` defaults to an empty table; validate supported configuration values and pass independent option data to each plugin invocation.
- Duplicate identities, duplicate setup calls, unknown fields, and ambiguous sources are configuration errors.
- An empty selection is valid and requires no directory.
- Startup declaration changes require restart initially.
- Missing packages produce installation guidance, not an automatic download.

## Plugin authoring contract

Existing script entrypoints returning nothing remain supported. New configurable entrypoints return a setup function:

```lua
return function(opts, plugin)
  local instruction = opts.instruction or "Review this code for bugs."

  plugin.action("insert_review", {
    scope = "promptbox",
    description = "Insert review instruction",
    run = function(ctx)
      ctx.promptbox.insert(instruction)
    end,
  })

  plugin.keys({
    insert_review = {
      action = "insert_review",
      key = "<F6>",
    },
  })
end
```

The action and context methods above are proposed interfaces, not existing APIs.

- Namespace action IDs by plugin identity, for example `review-helper.insert_review`.
- Binding IDs are stable plugin-local names independent of their default keys.
- Execute entrypoint and returned setup under the same ownership and staged registration boundary.
- Supplying nonempty options to a script that returns no setup function must fail clearly rather than discard options.
- Reject unsupported return values.
- Inline callbacks receive the same ownership context and options as returned setup functions.
- Preserve existing package-private helper-module semantics; do not clear global `package.loaded` during reload.
- Context operations must route through existing safe host/UI mechanisms, not expose mutable Rust state or block the TUI while waiting on its own actor.
- Specify action context availability and stale session/selection handling before exposing methods.

### Package layout

```text
rness-review-helper/
  rness-plugin.json
  plugin.lua
  lua/
    review_helper/
      text.lua
```

Retain the existing manifest's explicit entrypoint. Decide and test the API-version compatibility policy before shipping new authoring features; older binaries must not silently accept unsupported contracts.

## Keybinding contract

### Per-plugin overrides

```lua
keys = {
  insert_review = "<F8>",
  quote_message = false,
  close = { "<Esc>", "<F10>" },
}
```

- Omitted slots retain plugin defaults.
- Strings replace a slot's default key.
- Lists replace its entire key list.
- `false` disables a slot; `keys = false` disables all the plugin's default bindings.
- Disabling a binding does not disable its action and does not consume the old key. Other bindings may apply.
- Reject unknown slots, empty/invalid lists, duplicate normalized chords within a slot, and invalid key syntax.
- Binding scope derives from its registered action; reject unsupported scope changes.

### Central user overrides

```lua
rness.keymap.setup({
  {
    scope = "messagebox",
    key = "<F4>",
    action = "core.messagebox.toggle_tool",
  },
  {
    scope = "promptbox",
    key = "<F8>",
    action = "review-helper.insert_review",
  },
})
```

These declarations are resolved after runtime actions exist. Unknown actions fail validation before binding publication. Disabled or unloaded plugin actions must never leave callable stale references.

Central mappings and explicit per-plugin overrides occupy the same user layer. Identical target declarations may be deduplicated; competing explicit targets for the same scope/chord are errors, not load-order decisions.

### Scopes and precedence

- Core scopes: global, promptbox, messagebox.
- Plugin UI scope: `app:<registered-app-id>`, active only while the corresponding app owns focus.
- Modal UI captures input and prevents fallthrough into the underlying prompt or conversation. Preserve existing host recovery/cancellation behavior through an explicitly documented routing policy.
- Outside modal UI, check focused component scope before global scope.
- Within a scope, explicit user mappings override core and plugin defaults.
- Plugin defaults do not silently replace core bindings.
- Competing plugin defaults are suppressed with a diagnostic naming both owners.
- Two explicit user mappings that disagree are configuration errors.
- Scope selection precedes layer priority: a global user binding does not automatically defeat a local modal binding.
- Recompute effective bindings from declarations; never destructively mutate another owner's mapping.
- Initially support normalized single chords only. Reject multi-key sequences until timeout, prefix, cancellation, and text-input semantics are designed.
- Account for terminal-equivalent keys and supported protocol differences, including Tab/Ctrl-I and Escape/Alt ambiguity.

### Core and panel integration

- Register core promptbox/messagebox actions in the same action catalog.
- Integrate or explicitly migrate current component key configuration; do not retain competing dispatch systems.
- Replace plugin raw shortcut comparisons with named action dispatch where they represent configurable shortcuts.
- Keep ordinary text-input handling distinct from action bindings.
- Panel footer/help text must use resolved mappings, not hardcoded defaults.
- Actions should expose description, owner, scope, and effective bindings for diagnostics.

## Lifecycle and reload

Startup order:

1. Evaluate `init.lua` declarations.
2. Validate identities and resolve selected sources.
3. Mount required runtime APIs.
4. Stage ordered plugin execution, actions, and binding defaults.
5. Resolve user overrides and validate conflicts.
6. Synchronize registrations with engine and TUI.
7. Publish ready only after successful synchronization.

Reload:

- Watch explicitly selected development sources and their package helper roots, including external linked directories.
- Handle atomic-save rename events and refresh watch coverage when source files are replaced.
- Coalesce changes and defer reload during active work; process the pending reload when safe without requiring another save.
- Keep ordered whole-selection reload initially to preserve override and registration order.
- Stage actions, bindings, tools, apps, and other owned registrations together.
- Preserve the previous working registration generation if reload fails.
- Drop stale queued callbacks using registration-generation identity where needed.
- Keep a session-disabled identity set so `/unload` survives automatic reload until restart.
- Inline callback/declaration changes and managed package updates require restart initially.
- Document that runtime reload does not undo arbitrary external effects or shared Lua mutations.

## Installation and distribution workflow

Retain existing commands:

```sh
rness plugin link ./rness-review-helper
rness plugin list
rness plugin install https://github.com/example/rness-review-helper --rev v1.0.0
rness plugin update review-helper --rev v1.1.0
rness plugin remove review-helper --confirm
```

- Link for local package development; `file` for standalone scripts.
- Install records a resolved Git commit but does not activate code.
- Document that tags may move and full commit pins provide stronger reproducibility.
- Enable through an explicit package spec; restart to adopt setup changes or managed updates.
- Correct removal/update messaging to match actual checkout lifetime.
- Explain that captured Lua helpers do not preserve arbitrary files deleted by package update/removal. Recommend managing installed packages with affected sessions stopped until a stronger checkout-lifetime policy exists.
- Do not add build hooks, automatic dependency installation, or private-repository login flows in this change.

## Implementation phases

### Phase 1: Inventory and freeze contracts

- [ ] Trace current key routing from terminal events through core scopes, promptbox/messagebox, overlays, and Lua app handlers.
- [ ] Inventory repository plugin examples and legacy configuration shapes without modifying personal files.
- [ ] Identify actor/thread boundaries for callbacks and UI mutations.
- [ ] Finalize configuration schemas, API-version policy, failure behavior, and key normalization rules.
- [ ] Add baseline regressions for existing load order, ownership, reload, and unload.

### Phase 2: Explicit specs and deferred setup

- [ ] Introduce typed source/spec representation and config validation.
- [ ] Resolve exact file/package sources and config-relative paths.
- [ ] Support returned setup functions and deferred inline callbacks with ownership.
- [ ] Preserve script entrypoint support and package-private modules.
- [ ] Update startup loader wiring and ready ordering.

### Phase 3: Actions and binding resolution

- [ ] Add owned action and binding-slot registrations.
- [ ] Implement canonical chord parsing and deterministic scope/layer resolution.
- [ ] Implement per-plugin remap/disable and central user mappings.
- [ ] Stage bindings and actions with other runtime registrations.
- [ ] Add diagnostics for unresolved actions, unknown slots, conflicts, and shadowed defaults.

### Phase 4: TUI integration

- [ ] Register core actions and integrate existing key configuration.
- [ ] Dispatch focused scopes and modal scopes correctly.
- [ ] Implement the minimal action context methods needed by real migrated plugins.
- [ ] Migrate repository example shortcuts to named actions.
- [ ] Render effective key hints dynamically.
- [ ] Validate callback failure and stale-focus/session behavior without deadlocks.

### Phase 5: Reload and package lifecycle fixes

- [ ] Watch selected files and external linked package helper directories.
- [ ] Defer and coalesce changes during active turns.
- [ ] Keep session-unloaded plugins disabled across reload.
- [ ] Make action/binding publication and removal consistent with engine/UI synchronization.
- [ ] Correct installer lifecycle messages and document managed update/restart policy.

### Phase 6: Migration and documentation

- [ ] Replace repository `plugins.load(name)` examples with explicit specifications.
- [ ] Replace old selection API with an actionable migration error rather than silently supporting ambiguous resolution indefinitely.
- [ ] Document authoring, single-file development, package helpers, linking, publishing, installation, options, keys, unload, reload, and removal.
- [ ] Provide migration mappings for old host and component key settings.
- [ ] Provide a reference plugin covering promptbox, messagebox, and app-local actions.
- [ ] Keep personal plugin migration a separate permissioned operation.

### Phase 7: Validation and acceptance

- [ ] Run focused config/loader/package/runtime/keymap tests.
- [ ] Run workspace library/integration tests and project doc-tests.
- [ ] Run real-terminal QA under an isolated HOME and test package directory.
- [ ] Review final diff for scope, compatibility, documentation accuracy, and accidental personal/Claudio artifacts.

## Required test matrix

### Sources and setup

- Config-relative resolution independent of cwd; absolute paths; missing file/package.
- Duplicate identities/setup calls, invalid source combinations, disabled missing sources.
- Package/file names do not compete.
- Ordered script/setup/inline execution after APIs are ready.
- Options validation, invalid returns, setup failure, ownership and API compatibility.

### Input and bindings

- Default, remapped, multiple, disabled, and globally disabled plugin bindings.
- Stable binding IDs survive a changed plugin default key.
- Unknown slot/action and normalized duplicate chords fail clearly.
- Core/plugin conflicts, plugin/plugin conflicts, and explicit user conflicts match policy.
- Prompt typing is unaffected by messagebox shortcuts.
- Panel Enter never also submits the prompt; panel close can be remapped/disabled according to documented recovery policy.
- Dynamic hints reflect remaps and disabled slots.
- Terminal Escape/PageDown regressions remain passing.

### Reload and lifecycle

- Linked helper edits and atomic file replacement trigger reload.
- Active-turn changes eventually reload after becoming idle.
- Failed reload retains previous working registrations and mappings.
- Repeated reload does not duplicate bindings or leak callbacks.
- Unload removes owned actions/bindings and restores eligible lower layers.
- Session-unloaded plugins do not reactivate on file changes.
- Stale callbacks cannot run against an unloaded registration generation.
- Managed install/update never changes active bindings implicitly.

### End-to-end workflow

Use an isolated fixture package to exercise create → link → enable → invoke → remap → disable → reload → unload. Separately exercise installation from a controlled Git fixture, revision recording, update, and removal without modifying the user's environment.

## Definition of done

A developer can author and link a plugin, expose named UI actions with default bindings, and a user can install it, explicitly enable it, configure options, remap or disable its shortcuts, and unload it without orphaned bindings. Source resolution, focus handling, conflicts, errors, and reload behavior are deterministic and inspectable. Existing core UI behavior is covered by regressions. Documentation clearly distinguishes installation from activation and supported reload from arbitrary Lua side-effect rollback.

## Out of scope

- Lazy loading on keys/events/commands.
- Automatic repository discovery, dependency resolution, installation, or build scripts.
- Multi-key sequences, recursive mappings, or Vim modes.
- Sandboxing or total rollback of plugin effects.
- Automatically reloading `init.lua`.
- Automatic edits to personal configuration.

## References

- Neovim mapping scopes, precedence, and `<Plug>`: https://neovim.io/doc/user/map/
- Neovim keymap implementation: https://github.com/neovim/neovim/blob/master/runtime/lua/vim/keymap.lua
- Lazy key specifications: https://lazy.folke.io/spec/lazy_loading
- Lazy key resolution/handler implementation: https://github.com/folke/lazy.nvim/blob/main/lua/lazy/core/handler/keys.lua
