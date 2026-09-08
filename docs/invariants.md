# Invariants

Runtime-asserted contracts. Each has an owner module and CI coverage.
Violations are bugs by definition — fix the violator, never relax the
invariant without an ADR in `docs/decisions/`.

## Session log (rness-engine/session)

1. **Model-visible means logged.** Anything included in a model request is
   reconstructable from the session's event log. No hidden context.
   Asserted at request-build time.
2. **Append-only.** Committed events are never mutated or deleted. New
   truth = new events.
3. **Committed generations are never rewritten.** Migrations write
   `session.v(N+1).jsonl` beside `session.vN.jsonl`; old generations stay.
4. **Branch lineage is acyclic** and fork points reference events that
   exist in the parent's committed log.
5. **Attempts are preserved.** Failed/cancelled/retried model calls are
   logged as attempt events; they never appear in derived model context.

## Kernel (rness-kernel)

6. **Every effect is reversible.** Any registration made through the
   context has a disposer; plugin unload leaves no residue (listeners,
   services, mounts, timers).
7. **No privileged core.** Nothing bypasses the service/event seams;
   built-ins and Lua plugins use the same registration paths.

## Engine (rness-engine)

8. **Tool commits follow model order.** Parallel execution, deterministic
   history.
9. **Frames are never persisted.** Live streaming frames and durable
   events are disjoint types; only events reach the log.
10. **Interaction is UI-agnostic.** The engine never assumes which
    frontend answers an approval/question.

## Derived state

11. **Indexes are disposable.** Any cache/index (e.g. future SQLite) can
    be deleted and rebuilt from the logs with zero data loss.

## Frontends

12. **Frontends speak protocol only.** `rness-tui` and server clients
    import `rness-protocol`, never engine internals. Enforced by Cargo
    dependency direction.
