# Filesystem sandbox configuration

`rness.sandbox.setup` chooses the filesystem authority for Bash/process tools and built-in Write/Edit. It is independent from approval: an `allow` approval policy can run tools immediately while their filesystem policy still restricts writes.

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

Bash execution uses macOS Seatbelt, Linux Bubblewrap, or an explicitly configured Docker Desktop Linux-container image on Windows. See [process backend configuration](../../guides/execution-hardening.md#process-backend-configuration) for Lua options and platform requirements. Windows restricted commands use a Linux shell inside that image, not native PowerShell confinement. Missing backends fail closed; unrestricted Windows commands use the configured PowerShell executable. rness canonicalizes the workspace and work directory before launching.

Bash has OS-level filesystem confinement; built-in Write/Edit additionally enforce the mode through canonical path checks. These checks reject restricted writes outside the workspace, parent traversal, dangling/escaping symlinks and existing multiply linked files on Unix, but are not protection against hostile concurrent path swaps. See [execution hardening](../../guides/execution-hardening.md). This does **not** currently isolate network access, CPU or process counts, Lua/plugins/native extensions, or MCP-managed processes. Other host services reachable through IPC/network are not confined by the child's write policy, so do not treat this as a security boundary against hostile code. Reads (including secrets readable by your user) remain allowed. Pre-existing hard links inside a writable workspace can alias files outside it; writing through such a link changes the same inode and is not prevented by path-based Seatbelt rules. Use a trusted workspace without those aliases. A workspace is therefore a path-based write boundary, not a general-purpose container or VM.

The macOS backend allows `/dev/null` data writes in both restricted modes and
points `TMPDIR`, `TMP`, and `TEMP` at a private per-command directory. Linux uses
an isolated `/tmp` in workspace-write mode. Windows containers provide writable
`/tmp` only in workspace-write mode; Docker-managed `/dev` and `/dev/shm` remain
writable container-local special mounts even with a read-only root. Cleanup is
best-effort: crashes or an unavailable container daemon can leave temporary data
or containers behind. Windows artifact metadata does not have Unix directory-fsync
power-loss durability guarantees.

Implementation: [Lua declaration](../../../crates/rness-lua/src/api/config.rs), [durable session resolution](../../../crates/rness-engine/src/service.rs), and [Bash executor](../../../crates/rness-tools/src/sandbox.rs).
