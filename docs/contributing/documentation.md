# Documentation standards

## Language and organization

English is the canonical documentation language. Use portable Markdown, descriptive headings, and relative links. Organize material by reader need:

- `tutorials/`: a guided learning sequence with an observable result.
- `guides/`: a specific operational task.
- `reference/`: exact API and configuration contracts.
- `explanation/`: rationale and conceptual models.
- `contributing/`: development workflows.
- `project/`: limitations and compatibility information.
- `decisions/`: architectural decision records, clearly labeled by status.

Create a page when there is useful verified content. Do not scaffold empty pages merely to complete a directory tree.

## Page requirements

A tutorial or guide should state the outcome, prerequisites, steps, verification, and relevant failure cases. Clearly distinguish commands the user runs from model tool input and Lua plugin code.

A reference page should cover:

1. Availability and lifecycle phase.
2. Signature or data shape.
3. Field types and requiredness.
4. Omitted, empty, and default behavior.
5. Return values and errors.
6. Persistence and side effects.
7. Concurrency and authority restrictions.
8. A minimal example and related references.

Do not call a setter a patch operation unless it actually merges partial values. Do not describe an allowlist as a sandbox. Distinguish API parsing from model-provider support.

## Source of truth

Read the implementation before documenting a public contract. Trace important behavior through the service and adapter, not just API comments. Existing comments may describe an earlier design.

For configuration precedence, authentication, and permissions, prefer explicit test evidence. Record observable differences between entry points instead of describing intended uniformity as implemented behavior.

Link to relevant source files, not volatile line numbers. Keep tutorials concise by linking field tables to their canonical reference rather than duplicating them.

## Examples

- Never include real secrets.
- Explain every user-defined profile and connection name.
- State external prerequisites such as an installed model or running server.
- Avoid invented provider limits and claims about model availability.
- Keep complete copyable configurations in `examples/`; use focused snippets in documentation.
- Do not instruct users to overwrite existing personal configuration without review.
- Mark proposals as proposals; do not publish pending APIs in operational examples.

## Verification checklist

- [ ] Relative links resolve to existing files and valid headings.
- [ ] CLI examples agree with current `--help` and argument conflicts.
- [ ] Lua examples use implemented namespaces and correct lifecycle phases.
- [ ] Configuration examples deserialize where appropriate.
- [ ] External validation is distinguished from local tests.
- [ ] Limitations and migration notes are updated when behavior changes.
- [ ] Markdown has no trailing whitespace or accidental formatting changes.

Link checking, Markdown linting, and executable-example checks are recommended maintenance automation. This documentation does not imply those CI checks are already configured. Adding or changing CI should be a separately reviewed change.
