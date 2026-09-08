# Known limitations

This page records observed boundaries in the current checkout. It is not a roadmap or a promise of a release date.

## Configuration and Lua

- Production Lua changes require restart. Internal reload-capable test hosts do not establish production hot-reload support.
- Startup declarations close after `init.lua`; post-mount plugins cannot add providers, profiles, capabilities, or agents through those declaration methods.
- Lua module search retains existing `package.path` entries. Personal module paths are not a sandbox.
- Engine APIs are unavailable at startup top level. The `ready` hook should not be interpreted as a guarantee that all Lua tools are already synchronized.
- Legacy model-registry examples and process-global model exposure have not all been migrated to per-session configuration. Do not assume a statusline reports the active session's selected model or exact live context size.

## Agents

- `/agent <name>` is available through TUI and HTTP sends. There is no public `rness.agents.list()` Lua query.
- `rness.subagents.roster()` exposes delegable roles, not all principal roles or all child sessions.
- CLI principal snapshots do not share every validation step with service-based selection, including unknown-tool validation.
- Synchronous Lua delegation blocks the VM actor. Work requiring that same VM to execute callbacks may not be appropriate for this API.
- Allowed tool names are not a security sandbox. Trusted plugins and powerful tools can have broader effects than their names suggest.

## Providers and compatibility

- There is no complete maintained built-in model-capabilities catalog. Endpoint IDs and supported reasoning modes must be verified for your deployment.
- OpenAI-compatible describes a protocol family, not identical behavior across gateways.
- Local HTTP authentication tests verify adapter request behavior; they do not establish live OAuth entitlement or model availability.
- These pages target the source checkout. No release-level configuration or session-format compatibility guarantee is established by this documentation.

## Documentation coverage

The first documentation set covers installation, startup, profiles, agent roles, and delegation. Detailed reference for every Lua namespace, HTTP endpoint, event type, and built-in tool is still being written.

The original architecture document contains design intent alongside historical context. Consult current implementation and tests before treating an unreferenced architectural example as an available API.

When reporting a problem, include the revision, invocation, relevant redacted configuration, and observed error. Do not publish credentials or an entire session log without reviewing it for sensitive prompts, tool output, and workspace content.
