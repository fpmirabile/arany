# Library practice

Load for work in `crates/arany/`. Read the affected [domain contract](../docs/specs/README.md) and [security rules](./security.md) when triggered; the [system overview](../docs/architecture/system-overview.md#2-runtime-architecture) owns dependency direction and runtime responsibilities.

[Testing](./testing.md#corpus-placement) owns corpus placement and shared support; [development checks](../docs/development.md#verification) own commands.

## Engine loop decisions

Decide, then apply. A Run-loop decision (step end, stop cause, settlement, dispatch, child or worker outcome, Provider attempt) is a pure function over facts the shell already fetched or awaited, and it returns a closed enum. The effectful shell performs the awaits, appends and dispatches in journal-first order and only applies that value; the session reducer stays the sole evolve.

- `EngineError` means infrastructure failure only (journal, Store, coordinator, I/O). A refusal, limit, cancellation, pause, hook stop or unknown outcome is a decision variant, never a `bool`, an `Option` or an `Err` used for control flow.
- Match decision enums exhaustively, without a `_` arm, so a new cause fails to compile at every consumer.
- Pin each decision with a table test in its owner module; journeys keep proving the shell's ordering.

The [stop and settlement tables](../crates/arany/src/engine/run_loop/stop.rs) pin precedence; [Run-stop projection tests](../crates/arany/src/session/stop.rs) pin the committed facts shared by live and historical output.

## Growth

New code extends an owner without inflating it: when [global rule 8](../AGENTS.md#global-rules)'s split signals fire, add the phase or family child first and leave the parent the registration and dispatch line.

- A flow is `x.rs` plus `x/{phase}.rs`: the parent keeps the entry point and shared types, each child owns one phase and its focused tests with `pub(super)` or narrower visibility (`engine/run_loop/step.rs` with `step/{permission,ask,intent,reads,…}.rs`; `session/reducer/tool.rs` with `tool/{hook,lifecycle,permission}.rs`). A reducer `apply` or validator that gains an Event family gets a child.
- A new Tool operation adds one facts row in `tools/catalog.rs` and its family's arm in each consumer; no new `_` arm over `ToolCall` or `Family` variants, other than a fail-closed `false`/`None` in a pair comparison. List the consumers in the PR from `grep -rln 'ToolCall::' crates/*/src`.
- A decoder for a fixed-shape bounded document (Git host output, catalog, wire reply) reads byte-bounded input into a serde type, then checks field bounds in one validator; walk `serde_json::Value` only for open-shape data (streaming deltas, Tool arguments, MCP payloads).
- A per-dispatch setting joins the existing grouped owner (the consuming `ToolRuntime` builders, lifecycle owners) instead of adding a loose field plus a builder.
- A new limit is one named const beside its owner; one whose meaning is public is registered in `crates/arany/src/limits.rs` and linked from the spec per the [spec workflow](../docs/specs/README.md).
- A new focused test follows [corpus placement](./testing.md#corpus-placement) and [tests worth keeping](./testing.md#tests-worth-keeping).
