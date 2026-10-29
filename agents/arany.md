# Library practice

Practice for `crates/arany/`; read only the section your edit needs. Open the affected [spec](../docs/specs/README.md) section and [security rules](./security.md) only when triggered; the [system overview](../docs/architecture/system-overview.md#2-runtime-architecture) owns dependency direction and runtime responsibilities.

[Testing](./testing.md#corpus-placement) owns corpus placement and shared support; [development checks](../docs/development.md#verification) own commands.

## Skills

Load the `rust-clean-code` skill after Ponytail for Rust choices and `clean-architecture-review` for architecture reviews, as [priority 3](../AGENTS.md#product-and-implementation-priorities) says; [global rules](../AGENTS.md#global-rules) override softer skill wording. Where they bite Rust:

- Comments follow [global rule 2](../AGENTS.md#global-rules), including no `///` in internal code, so Clippy's `missing_errors_doc`/`missing_panics_doc` stay allowed; `// SAFETY:` is one line, spilling to a second only for a distinct obligation.
- Errors use `thiserror`, never `anyhow`; `Provider` is held as `Arc<dyn Provider>` and stays on `async-trait`.
- A new Clippy allowance is `#[expect(..., reason = "...")]` and needs review. Code targets the `rust-version` and edition 2024 in `Cargo.toml`; CI runs the toolchain pinned in `rust-toolchain.toml`.
- Test helpers live in `crates/arany/src/test_support/`; [testing](./testing.md) owns determinism and test ownership over the skill's test advice.
- Architecture reviews load the owning spec section and the tests that pin the module, and use the deep-module vocabulary of [global rule 3](../AGENTS.md#global-rules) beside the skill's lenses. SQLite ownership and the separate credential and Guard processes have real lifecycle and privilege reasons, and Provider already has several adapters.

The owner is learning Rust while building: when a Rust mechanism decides a change, explain it in one plain sentence, and for a significant choice say what was chosen, why it fits and when the alternative would. Keep the lesson in the conversation, never in source comments; routine edits need none.

## Module boundaries

Before changing a boundary, read the [system overview](../docs/architecture/system-overview.md#2-runtime-architecture) section that owns crate and process shape, dependency direction and state ownership, and the owning spec section. Create a module only once it has depth; a crate needs measured dependency, release, privilege, ownership or compile pressure. Add a daemon only for a second Client or detached execution. Effectful Tools require deterministic Policy and the separate Guard, except for the primary host operations the [per-effect isolation guarantee](../AGENTS.md#non-negotiable-guarantees) authorizes.

## Engine loop decisions

Decide, then apply. A Run-loop decision (step end, stop cause, settlement, dispatch, child or worker outcome, Provider attempt) is a pure function over facts the shell already fetched or awaited, and it returns a closed enum. The effectful shell performs the awaits, appends and dispatches in journal-first order and only applies that value; the session reducer stays the sole evolve.

- `EngineError` means infrastructure failure only (journal, Store, coordinator, I/O). A refusal, limit, cancellation, pause, hook stop or unknown outcome is a decision variant, never a `bool`, an `Option` or an `Err` used for control flow.
- Match decision enums exhaustively, without a `_` arm, so a new cause fails to compile at every consumer.
- Pin each decision with a table test in its owner module; journeys keep proving the shell's ordering.

The [stop and settlement tables](../crates/arany/src/engine/run_loop/stop.rs) pin precedence; [Run-stop projection tests](../crates/arany/src/session/stop.rs) pin the committed facts shared by live and historical output.

## Growth

New code extends an owner without inflating it: when [global rule 8](../AGENTS.md#global-rules)'s split signals fire, add the phase or family child first and leave the parent the registration and dispatch line.

- A flow is `x.rs` plus `x/{phase}.rs`: the parent keeps the entry point and shared types, each child owns one phase and its focused tests with `pub(super)` or narrower visibility (`engine/run_loop/step.rs` with `step/{permission,ask,intent,reads,…}.rs`; `session/reducer/tool.rs` with `tool/{hook,lifecycle,permission}.rs`). A reducer `apply` or validator that gains an Event family gets a child.
- A new Tool operation adds one facts row in `tools/catalog.rs` and its family's arm in each consumer; no new `_` arm over `ToolCall` or `Family` variants, other than a fail-closed `false`/`None` in a pair comparison. List the consumers in the PR from `rg -l 'ToolCall::|Family::' crates/*/src`.
- A decoder for a fixed-shape bounded document (Git host output, catalog, wire reply) reads byte-bounded input into a serde type, then checks field bounds in one validator; walk `serde_json::Value` only for open-shape data (streaming deltas, Tool arguments, MCP payloads).
- A per-dispatch setting joins the existing grouped owner (the consuming `ToolRuntime` builders, lifecycle owners) instead of adding a loose field plus a builder.
- A new limit is one named const beside its owner; one whose meaning is public is registered in `crates/arany/src/limits.rs` and linked from the spec per the [spec workflow](../docs/specs/README.md).
- A new focused test follows [corpus placement](./testing.md#corpus-placement) and [tests worth keeping](./testing.md#tests-worth-keeping).
