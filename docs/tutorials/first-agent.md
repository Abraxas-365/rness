# Your first agent

**Outcome:** define a principal-only role and a delegable role without duplicating model settings.

## Prerequisites

Complete [First configuration and session](first-configuration.md). The commands below use its `local-qwen` profile. The chosen model must support tool calls to delegate autonomously.

## 1. Declare two roles

Add to `~/.rness/init.lua`:

```lua
rness.agents.declare("architect", {
  description = "Discusses requirements and designs an implementation plan.",
  instructions = "Clarify the task and discuss tradeoffs before implementing changes.",
  subagent = false,
})

rness.agents.declare("worker", {
  description = "Implements a scoped task and reports test results.",
  instructions = "Follow project conventions. Complete only the assigned task and report verification results.",
  subagent = true,
})
```

Both roles can be selected as the principal agent. Only `worker` can be requested by name through delegation. Omitting `subagent` has the same effect as `false`.

Neither role specifies `profile`: selecting the principal preserves the session's generation settings, and a named child inherits its parent's generation settings.

## 2. Select the principal

Restart rness with:

```sh
rness --profile local-qwen --agent architect --approval ask
```

To select this role by default for new sessions, add:

```lua
rness.default_agent = "architect"
```

Defaults do not automatically replace the saved role when resuming a session. In the TUI, type `/agent` and press Tab to complete a declared role, then submit `/agent architect`. Selection requires an idle session and persists the resolved role without starting a model turn. The same command is accepted through the HTTP send endpoint.

## 3. Request a small delegation

Ask the principal to delegate a bounded task to `worker`, such as reviewing a function and reporting potential edge cases without editing files. This is a request to the model, not a guarantee that it will invoke a tool.

The corresponding model-facing tool input is:

```json
{
  "provider": "spawn",
  "agent": "worker",
  "prompt": "Review the specified function for edge cases. Report findings without editing files."
}
```

Here `provider` means the child-creation mechanism, not the model connection. `spawn` starts without the parent's conversation. Include enough task context and actual file paths in the prompt. `fork` instead inherits the parent's completed-turn prefix.

A foreground success returns the child's session ID and final assistant text. The child also appears in the session listing.

## 4. Understand the permission boundary

Instructions such as “do not edit” are behavioral guidance, not enforced permissions. To prohibit tools for a role, use `tools = {}`. A nonempty list must use actual registered tool names.

A child receives a durable tool ceiling derived from the parent's effective allowed tools. Choosing a broader role cannot remove that ceiling. An allowlist is not a sandbox: a permitted shell tool may still perform broad filesystem or network operations.

## Next steps

- [Agent fields and validation](../reference/configuration/agents.md).
- [Delegation from Lua](../reference/lua/subagents.md).
- [The model-facing subagent tool](../reference/tools/subagent.md).
