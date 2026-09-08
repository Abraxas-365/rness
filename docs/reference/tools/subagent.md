# Subagent tool reference

`subagent` delegates a task from its calling session. It requires session-aware dispatch; calling it without a session fails.

## Input

| Field | Type | Required | Behavior |
| --- | --- | --- | --- |
| `provider` | string | Yes | `spawn` for fresh conversation; `fork` for completed parent history |
| `prompt` | string | Yes | Task instructions for this activation |
| `agent` | string | No | Declared role enabled with `subagent = true` |
| `run_in_background` | boolean | No | Defaults to false |
| `background_mode` | string | No | `one-shot` or `continuable` |

The generated schema includes enabled agent names and descriptions. With an empty roster, it omits the `agent` property. Runtime validation still rejects disabled or unknown names if supplied manually.

## Foreground

```json
{"provider":"spawn","agent":"worker","prompt":"Inspect the requested files and report findings."}
```

Waits for the child's turn to settle. Success returns its session ID and final assistant text. Aborted or failed runs become error tool results, allowing the principal to decide how to continue.

## Background one-shot

```json
{
  "provider":"spawn",
  "agent":"worker",
  "prompt":"Inspect the requested files and report findings.",
  "run_in_background":true,
  "background_mode":"one-shot"
}
```

Returns a job ID. Observe it with `job_output`. Role validation occurs inside the background task, so a returned job ID does not prove that child creation succeeded; inspect the job's result.

## Continuable

```json
{
  "provider":"spawn",
  "agent":"worker",
  "prompt":"Inspect the task and wait for follow-up work.",
  "run_in_background":true,
  "background_mode":"continuable"
}
```

Returns a durable child session ID. Use `send_message`, `interrupt_agent`, and `list_agents` for subsequent interaction. Settled child turns produce notices in the parent.

Current parsing treats `background_mode = "continuable"` as sufficient to select this path even if `run_in_background` is omitted. Supply both fields to make intent explicit. Do not infer strict schema validation for every optional field from the advertised JSON Schema.

## Context and permissions

`fork` uses the completed-turn prefix, not the parent's currently unbalanced tool-calling turn. Include task-specific details that may exist only in that in-flight turn. A turnless fork falls back to fresh history.

Named role selection does not control how context is inherited: `agent` and `provider` are independent choices. Both mechanisms preserve generation settings unless the selected role has a profile. Both enforce the parent's captured tool ceiling.

See [Lua delegation](../lua/subagents.md) and [agent configuration](../configuration/agents.md).

Implementation: [tool](../../../crates/rness-tools/src/subagent.rs), [runtime](../../../crates/rness-engine/src/subagent.rs).
