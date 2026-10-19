# Library practice

Load for work in `crates/arany/`. Read the affected [domain contract](../docs/specs/README.md) and [security rules](./security.md) when triggered; the [system overview](../docs/architecture/system-overview.md#2-runtime-architecture) owns dependency direction and runtime responsibilities.

[Testing](./testing.md#corpus-placement) owns corpus placement and shared support; [development checks](../docs/development.md#verification) own commands.

## Engine loop decisions

Decide, then apply. A Run-loop decision (step end, stop cause, settlement, dispatch, child or worker outcome, Provider attempt) is a pure function over facts the shell already fetched or awaited, and it returns a closed enum. The effectful shell performs the awaits, appends and dispatches in journal-first order and only applies that value; the session reducer stays the sole evolve.

- `EngineError` means infrastructure failure only (journal, Store, coordinator, I/O). A refusal, limit, cancellation, pause, hook stop or unknown outcome is a decision variant, never a `bool`, an `Option` or an `Err` used for control flow.
- Match decision enums exhaustively, without a `_` arm, so a new cause fails to compile at every consumer.
- Pin each decision with a table test in its owner module; journeys keep proving the shell's ordering.

The [Engine loop plan](../planning/engine-loop/README.md) migrates the current loop to this rule.
