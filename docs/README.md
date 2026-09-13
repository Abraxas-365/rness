# rness documentation

rness is a Rust coding-agent runtime with Lua configuration and extensibility, a terminal interface, and an HTTP/SSE service interface.

These pages describe the current source checkout, not a versioned stable release. Configuration and extension APIs are evolving. See [known limitations](project/known-limitations.md) before relying on behavior in an integration.

## Choose a path

| Goal | Start here |
| --- | --- |
| Build and run rness | [Install from source](guides/installation/from-source.md) |
| Configure your first connection | [First configuration and session](tutorials/first-configuration.md) |
| Define a principal agent or delegate a task | [Your first agent](tutorials/first-agent.md) |
| Organize personal Lua configuration | [The init.lua entry point](guides/configuration/init-lua.md) |
| Understand providers, profiles, and roles | [Configuration concepts](explanation/providers-models-profiles-agents.md) |
| Look up agent fields | [Agent configuration reference](reference/configuration/agents.md) |
| Opt into Bash filesystem confinement | [Sandbox configuration](reference/configuration/sandbox.md) |
| Call delegation APIs from Lua | [Lua subagents reference](reference/lua/subagents.md) |
| Check which model settings win | [Configuration precedence](reference/configuration-precedence.md) |
| Use slash commands and input history | [TUI commands](guides/configuration/tui-commands.md) |
| Configure and switch colors | [Colorschemes](guides/configuration/colorschemes.md) |
| Install Lua packages | [Plugin packages](guides/plugins/packages.md) |
| Load, reload, and unload plugins | [Plugin lifecycle](guides/plugins/loading-and-lifecycle.md) |
| Define actions, remap keys, and inspect help | [Scoped mappings](guides/plugins/loading-and-lifecycle.md#scoped-mappings-and-help) |
| Customize messages and tool cards | [Messagebox presentation](guides/plugins/example-recipes.md#messagebox-presentation) |
| Register commands and use session context | [Lua commands](reference/lua/commands.md) |
| Write native extensions | [Native extension contracts](contributing/native-extensions.md) |
| Contribute to the codebase | [Contributor documentation](contributing/README.md) |

## How these docs are organized

- **Tutorials** teach a complete workflow with a concrete outcome.
- **Guides** explain how to accomplish a particular task.
- **Reference** specifies fields, signatures, errors, and operational constraints.
- **Explanation** describes concepts and architectural tradeoffs.
- **Contributing** covers development and documentation maintenance.
- **Project** records current limitations and compatibility qualifications.
- **Decisions** preserves architectural decision records.

The first documentation set focuses on startup configuration and named agents. More API coverage will be added as its behavior is checked against source and tests; missing pages are not represented by empty placeholders.

## Examples and historical design

[examples/init.lua](../examples/init.lua) is a copyable configuration, not an automatically loaded project configuration. Read and adapt it before installing it under your home directory. Never overwrite an existing personal configuration without reviewing the changes.

The original [architecture document](architecture.md) and [invariants](invariants.md) remain available. The founding architecture describes design intent; it is not a guarantee that every described subsystem or API is implemented.

## Documentation scope

Examples use English and Markdown that can be read directly on GitHub. Endpoint model IDs are examples, not a maintained provider catalog. Use IDs supported by your account or local server. Never put real credentials in documentation, examples, or issue reports.
