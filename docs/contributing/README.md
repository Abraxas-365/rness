# Contributing documentation

## Start with the workspace

From the repository root:

```sh
cargo check --workspace --tests
cargo test --workspace
```

Use targeted tests while iterating, then run the workspace suite before reporting a cross-crate change as verified. Do not replace real authentication tests with live credentials in committed fixtures.

## Repository responsibilities

| Crate | Primary responsibility |
| --- | --- |
| `rness-kernel` | Domain-independent plugin and event infrastructure |
| `rness-protocol` | Shared durable and client-facing data types |
| `rness-engine` | Sessions, replay, turns, permissions, and delegation mechanics |
| `rness-providers` | Model transports, authentication, and adapter mappings |
| `rness-tools` | Built-in tool implementations and model-facing delegation |
| `rness-lua` | Lua VM host, startup declarations, and extension bridges |
| `rness-mcp` | MCP integration |
| `rness-tui` | Terminal frontend |
| `rness-server` | HTTP/SSE frontend |
| `rness-cli` | Application composition and CLI startup |

Check the [invariants](../invariants.md) before changing session persistence or request derivation. Keep engine mechanisms separate from Lua configuration policy; do not make frontend code bypass the service seam to mutate durable state.

## Relevant regression suites

- `crates/rness-engine/tests/subagent.rs`: child creation, lineage, role eligibility, and inherited configuration.
- `crates/rness-engine/tests/turn_loop.rs`: request construction and tool dispatch.
- `crates/rness-engine/tests/service.rs`: durable service configuration and lifecycle.
- `crates/rness-tools/tests/subagent_tool.rs`: model-facing dispatch and job behavior.
- `crates/rness-lua/src/api/config.rs`: startup declarations and retained-VM behavior.
- `crates/rness-providers/tests/routes_auth.rs`: protocol-specific authentication headers over local HTTP.

## Before submitting a change

1. Explain the user-visible behavior and the affected boundary.
2. Add a regression that fails without the change where practical.
3. Check durable schema changes for replay and resume consequences.
4. Update examples and reference pages if fields or signatures change.
5. Report which checks ran and which external behavior remains unverified.

Follow the [documentation standards](documentation.md) when adding or updating pages.
