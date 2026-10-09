# Subagent tool reference

`subagent` delegates a task from its calling session. It requires session-aware dispatch; calling it without a session fails.

## Input

| Field | Type | Required | Behavior |
| --- | --- | --- | --- |
| `provider` | string | Yes | `spawn` for fresh conversation; `fork` for completed parent history |
| `prompt` | string | Yes | Task instructions for this activation |
| `agent` | string | Unless generic children are enabled | Declared role enabled with `subagent = true` |
| `run_in_background` | boolean | No | Defaults to false |
| `background_mode` | string | No | `one-shot` or `continuable` |

The generated schema includes enabled agent names and descriptions. By default `agent` is required: generic children are disabled. Set `rness.agents.allow_generic = true` at startup to allow omission. With an empty roster and generic children enabled, the schema omits the `agent` property; otherwise an empty roster marks delegation unavailable. Runtime validation rejects unnamed requests when disabled and always rejects disabled or unknown role names. If no configured role fits, do not delegate.

## Foreground

```json
{"provider":"spawn","agent":"worker","prompt":"Inspect the requested files and report findings."}
```

Waits for the child's turn to settle. Success returns its session ID and final assistant text. Aborted or failed runs become error tool results, allowing the principal to decide how to continue. A failed run's result includes the child's last provider error (code and message, up to 500 characters) when the child produced no text.

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

Returns a job ID. Observe it with `job_output`. Role and generic-child policy validation occurs before accepting a background job. A returned job ID still does not prove child creation or execution succeeded; inspect the job's result.

`job_kill` cancels the child's turn (it ends `cancelled`) and the job settles as `killed`, with one completion notice. The kill also stops the background subagents that child started. Its background Bash jobs are not stopped: they keep running until they finish or the rness process exits. Their completion notices are logged to the killed child without waking it. A kill that lands before the child exists creates no child.

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

## Teardown

When the TUI exits, it tears down its session. The cancel reaches every delegated descendant: running child turns end `cancelled`, background subagent jobs of the session settle `killed`, and all jobs owned by descendants (Bash, terminal and subagent) are stopped. Continuable children remain resumable sessions. Settle and job notices that arrive during teardown are logged without waking the parent.

## Host crash

If the rness process dies while children are running, their turns are left open in the logs. The next rness that opens or resumes the session (TUI start or `-s`, or the first turn a `--serve` host runs in that tree) closes those turns as `cancelled`. Each continuable child's parent gets one `[subagent <id> settled: interrupted]` notice. The notice is logged without starting a turn; a turn running there sees it at its next step. Only the tree of the session being opened is reconciled. Other sessions in the store are left alone, since another rness process may be hosting them. Children whose open turn started more than 16 MiB before the end of their log are not detected. Headless `-p` runs skip this step.

Current parsing treats `background_mode = "continuable"` as sufficient to select this path even if `run_in_background` is omitted. Supply both fields to make intent explicit. Do not infer strict schema validation for every optional field from the advertised JSON Schema.

## Context and permissions

`fork` uses the completed-turn prefix, not the parent's currently unbalanced tool-calling turn. Include task-specific details that may exist only in that in-flight turn. A turnless fork falls back to fresh history.

Named role selection does not control how context is inherited: `agent` and `provider` are independent choices. Both mechanisms preserve generation settings unless the selected role has a profile. Both enforce the parent's captured tool ceiling.

See [Lua delegation](../lua/subagents.md), [agent configuration](../configuration/agents.md), and the [workflow tool](workflow.md) for scripted fan-out across many children.

Implementation: [tool](../../../crates/rness-tools/src/subagent.rs), [runtime](../../../crates/rness-engine/src/subagent.rs).
