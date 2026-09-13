# Filesystem sandbox configuration

`rness.sandbox.setup` chooses the filesystem authority for Bash/process tools. It is independent from approval: an `allow` approval policy can run Bash immediately while the operating-system sandbox still restricts its writes.

```lua
rness.sandbox.setup({
  default = "workspace-write",
  agent_overrides = "tighten-only",
  unavailable = "deny",
})

rness.agents.declare("reviewer", {
  description = "Inspects changes without editing files.",
  instructions = "Review only; do not modify files.",
  sandbox = "read-only",
})

rness.agents.declare("worker", {
  description = "Implements changes inside the session workspace.",
  instructions = "Make and verify the requested change.",
  subagent = true,
  sandbox = "workspace-write",
})
```

## Modes

| Mode | Filesystem effect |
| --- | --- |
| `read-only` | Bash may read but filesystem writes are denied. |
| `workspace-write` | Writes are allowed under the canonical, immutable session workspace and an executor-owned temporary directory. |
| `danger-full-access` | Bash has the normal authority of the rness process user. |

Sandboxing is **off by default**. With neither a global setup nor an agent `sandbox` override, Bash uses `danger-full-access`, creates no sandbox temporary directory, and new sessions omit the sandbox field. The shipped flavor does not enable it. The example above is opt-in; `rness.sandbox.setup({})` also retains full access.

A role can opt into `sandbox = "read-only"` without a global setup; other roles and ordinary sessions remain unrestricted. Restricted modes require a session workspace (the CLI supplies its current directory by default). Headless callers must supply a workspace or configure the service default; missing boundaries are rejected rather than silently bypassed.

## Scope and policy precedence

The global `default` applies to **new** ordinary model sessions, including configurations with no named agent. A named agent may set `sandbox` only to the same mode or a stricter mode; a startup configuration that broadens it is rejected. Named and unnamed delegated children retain their parent's restrictions; a role can narrow them further.

The effective mode is durable. Resume and fork retain it even if `init.lua` changes. A historical fork also inherits the parent's current restriction, even if the selected history predates that restriction. Model/profile changes and generic configuration updates must not erase it. Selecting another role cannot loosen an already restricted session. To use a broader policy, explicitly configure it and start a new session. Existing unrestricted sessions do not become restricted merely because the startup default changed.

`agent_overrides = "tighten-only"` and `unavailable = "deny"` are currently the only supported values. They make two security properties explicit:

- models cannot relax a session's sandbox; and
- an enforced mode never falls back to unrestricted shell execution when its backend is unavailable.

`rness.sandbox.setup` is a startup-only declaration. Calling it more than once, or after startup declarations close, is an error and requires a restart.

## Enforcement and current limits

Bash execution is enforced on macOS using `/usr/bin/sandbox-exec` (Seatbelt), including descendant processes. rness canonicalizes both the workspace and requested work directory before launching. On platforms without an implemented backend, `read-only` and `workspace-write` fail closed with an explanatory tool error; `danger-full-access` continues to use the ordinary shell.

This is filesystem confinement for Bash/process tools only. It does **not** currently isolate network access, CPU or process counts, Lua/plugins/native extensions, MCP-managed processes, or non-Bash tools. Other host services reachable through IPC/network are not confined by the child's write policy, so do not treat this as a security boundary against hostile code. Reads (including secrets readable by your user) remain allowed. Pre-existing hard links inside a writable workspace can alias files outside it; writing through such a link changes the same inode and is not prevented by path-based Seatbelt rules. Use a trusted workspace without those aliases. A workspace is therefore a path-based write boundary, not a general-purpose container or VM.

`/dev/null` accepts data writes in both restricted modes. `TMPDIR`, `TMP`, and `TEMP` point at a private per-command directory; only `workspace-write` allows writing there. Cleanup is best-effort: process crashes or files deliberately made undeletable can leave temporary data behind.

Implementation: [Lua declaration](../../../crates/rness-lua/src/api/config.rs), [durable session resolution](../../../crates/rness-engine/src/service.rs), and [Bash executor](../../../crates/rness-tools/src/sandbox.rs).
