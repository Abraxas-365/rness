# Providers, models, profiles, and agents

These concepts answer different questions. Keeping them separate lets a role work across models without duplicating connection details.

| Concept | Question answered | Example |
| --- | --- | --- |
| Provider connection | Where and how do requests go? | OpenAI-compatible endpoint with an environment API key |
| Model selection | Which upstream model should receive the request? | Connection `router`, model ID `anthropic/...` |
| Capabilities | What facts have been declared about that pair? | Context limit or supported reasoning efforts |
| Profile | Which model and generation preferences should be reused? | `coding` with an output limit |
| Agent | What role should perform the work, with which tools? | `reviewer` with review instructions |
| Session configuration | What resolved settings actually apply to this conversation? | Durable selection, role snapshot, and request options |

## Why a profile is not a role

A profile selects generation behavior. A role defines instructions and optional tool restrictions. Combining both into one mandatory declaration would force separate role copies for each model.

An agent may reference a profile when a particular role needs a specific model. Omitting the profile deliberately leaves the generation choice with the current session or delegating parent.

## Why declaring a role does not enable delegation

Some roles are intended only for direct interaction with the user. Registering such a role should not implicitly let the model invoke it as a child. `subagent = true` makes that exposure explicit. The runtime checks the flag, not just the tool UI.

## Why a session keeps a snapshot

A session should not silently change personality when a user edits a declaration between runs. Persisting the effective instructions and tool list makes resumption independent of registry edits. Explicitly selecting a role again uses the current declaration.

This does not make the entire request environment immutable: connection definitions, adapter behavior, base system configuration, and tool implementations still belong to the running process. A role snapshot is not a reproducible deployment artifact by itself.

## Why delegation needs two permission layers

The child's role allowlist expresses what that role needs. The inherited ceiling expresses what the parent was allowed to delegate. Effective tools are the intersection of available registered tools, the ceiling, and any role restriction.

Removing a role restriction must not remove the inherited ceiling. Conversely, the ceiling does not isolate a process or constrain all effects of a permitted tool. See [agent permissions](../reference/configuration/agents.md#tool-enforcement).

## Why spawn and fork are separate from roles

A role answers who performs the task. Spawn and fork answer which conversation context the child starts with. The same worker can start fresh or inherit completed history without a second declaration.

See [precedence](../reference/configuration-precedence.md) for operational rules and [the first-agent tutorial](../tutorials/first-agent.md) for a concrete workflow.
