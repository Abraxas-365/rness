# Workflows: fan work out across many subagents

This guide shows how to get the model to orchestrate a large, parallel task with the [`workflow`](../reference/tools/workflow.md) tool, how to follow the run, and what to expect when it fails.

## Prerequisites

- Delegation is available. Either at least one role is declared with `subagent = true`, or `rness.agents.allow_generic = true` is set. See [agent configuration](../reference/configuration/agents.md). Without either, the tool refuses every call.
- [Tool exposure](plugins/example-recipes.md#deferred-tools-and-programmatic-tool-calling) is `native` (the default) or `both`. `ptc` mode hides the tool.

## When a workflow fits

A workflow suits work made of many independent pieces with a mechanical merge at the end:

| Task | Shape |
| --- | --- |
| Audit many files or crates | `pipeline(files, scan, verify)`, each stage using `schema` |
| Migration | `pipeline(files, migrate, check)`, where each item is fixed and then checked on its own |
| Review or research from several angles | `parallel({perf, security, design})`, then one `agent()` that combines the results |
| Adversarial verification | Find issues, then have a skeptical member try to disprove each one; return only the findings that hold up |

For one or two delegated tasks, the plain [`subagent`](../reference/tools/subagent.md) tool is simpler.

## Ask for one

Name the tool and describe the fan-out. For example:

> Use the workflow tool to audit every crate under `crates/` for `unwrap()` in non-test code. Use one scout per crate with a schema of `{file, line, why}`, then have a reviewer verify each crate's findings. Return only the confirmed findings.

The model writes a script similar to the example in the [reference](../reference/tools/workflow.md#example). When roles are configured, it can pass `role = "<name>"` to each `agent()` call.

## Follow the run

The tool call renders as a live card that updates about five times a second:

```text
workflow: unwrap-audit · running · 42s
Audit crates for unwrap in non-test code
Phase: Verify
Agents: 14 · 3 running · 2 queued · 8 done · 1 failed · 0 cancelled
  ✓ rness-engine · Scan
  ● rness-engine verify · Verify
  …
› scanned 7 crates
```

Each member is an ordinary child session. Select one in the agent monitor to read its transcript. When the run finishes, the card shows the final counts and the result, and the model receives only the script's return value.

## Stop a run

Interrupting the parent turn, for example with Ctrl+C, cancels the script and every running member. The card changes to `cancelled`, and the model receives an error result saying the workflow was cancelled.

## Failure cases

| What you see | Meaning |
| --- | --- |
| Some items are `null` in the result | Those members failed, were cancelled, or never reported their structured result. The run itself succeeded. |
| `failed after starting N agents: …` | The script misused the API (for example an unknown `agent()` option, a bad schema, an unknown role, or a tripped cap) or raised an uncaught error. Members that were still running were cancelled. |
| `Workflows are unavailable…` | No delegation is possible. See Prerequisites. |
| Result ends with `[workflow result truncated …]` | The value exceeded `max_result_chars`. Ask the model to return a summary instead. |

To adjust concurrency and other caps, see [`rness.workflow` limits](../reference/tools/workflow.md#execution-and-limits). Runs are foreground only and are not resumed after a restart. See [known limitations](../project/known-limitations.md).
