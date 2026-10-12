# Rust foundation: runtime and ownership rationale

**Research baseline:** 2026-09-29.

[The system overview](../architecture/system-overview.md) owns physical modules
and interfaces. [Execution](../specs/execution.md), [Session rules](../../agents/session.md)
and [Provider rules](../../agents/provider.md) own semantics. This report retains
why the runtime and integration choices fit the harness.

## One coordinator, explicit blocking owners

Provider calls spend much of their time waiting on sockets and timers. A
current-thread Tokio coordinator keeps async ownership and cancellation direct;
a larger scheduler does not shorten remote generation. CPU work or runnable-task
contention must be measured before changing that choice.

Synchronous SQLite work has a different lifetime. One named thread owns one
connection and receives bounded requests. This keeps blocking commits off the
coordinator and makes transaction, reply and shutdown ownership explicit.
A connection pool or SQLx would add concurrency and abstraction that this
single-writer workload does not require.

A dedicated store owner also avoids putting an enduring connection into
Tokio's generic blocking pool. Started blocking work cannot simply be aborted.
This is a reason to bound and supervise it, not a blanket prohibition on
`spawn_blocking`: credential and native Guard lifecycles have their own justified
blocking or process boundaries in the architecture.

## Keep the Engine seam semantic

The Engine accepts domain outcomes rather than HTTP responses, vendor SDK
objects or parameter bags. Provider variation justifies one substitutable
behavior seam. Direct bounded HTTP adapters keep request destinations, output
limits and strict decoding visible; a community SDK's retry or routing defaults
must not silently become product policy.

Session state, context selection, scheduling and SQLite remain concrete private
owners. Testing a real file-backed database gives stronger durability evidence
than adding a repository trait solely to mock queries. Pure reducers and
presentation values can be exercised without a renderer framework in the Engine.

## Identity and ordering solve different problems

Typed identifiers prevent confusing Sessions, Runs and AgentRuns. Journal
sequence determines canonical order; wall timestamps serve display; monotonic
time serves deadlines. A generic Clock or ID-generation trait is unnecessary
unless actual variation earns it. The current types and Event schema live in
the architecture and source, not illustrative declarations here.

## Bounds and cancellation belong to owners

Bounded channels keep memory use and backpressure visible. A journal request
already accepted by its owner can finish committing even if its caller stops
waiting, so cancellation must not fabricate a rollback. Commit acknowledgement
precedes feedback; interrupted replay describes an honest committed prefix.
[Store rules](../../agents/store.md) and [Engine rules](../../agents/engine.md)
own the exact ordering and admission requirements.

Telemetry is a separate lossy output owner. Its SDK worker and shutdown must
fit the coordinator lifetime without blocking Run correctness. See
[OTLP rationale](./otlp-observability.md) and [telemetry rules](../../agents/telemetry.md).

## Maintenance

[Cargo.toml](../../Cargo.toml), [Cargo.lock](../../Cargo.lock) and the
[dependency inventory](../security/beta-dependency-inventory.md) own the reviewed
build inputs. Do not maintain another dependency version/feature table here.
[Testing rules](../../agents/testing.md) own verification; [next steps](../../NEXT_STEPS.md)
owns missing evidence. Future Client or crate boundaries are discussed in
[modular architecture](./modular-harness-architecture.md).

## Primary sources

- Tokio: [runtime](https://docs.rs/tokio/latest/tokio/runtime/), [bridging](https://tokio.rs/tokio/topics/bridging), [bounded channels](https://tokio.rs/tokio/tutorial/channels), [blocking-task lifetime](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)
- Rusqlite: [connection ownership](https://docs.rs/rusqlite/latest/rusqlite/struct.Connection.html), [bundled SQLite](https://github.com/rusqlite/rusqlite)
- [SQLx pool](https://docs.rs/sqlx/latest/sqlx/type.SqlitePool.html)
- OpenAI: [Structured Outputs](https://developers.openai.com/api/docs/guides/structured-outputs), [Responses reference](https://developers.openai.com/api/reference/resources/responses/methods/create)
