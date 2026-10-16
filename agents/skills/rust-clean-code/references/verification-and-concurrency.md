# Verification and concurrency

## Test a claim

A test owns one claim: the observable behavior or failure it protects. State the defect it would catch; if it cannot be shown to fail for that defect, strengthen an existing test, type or parser instead. Reuse the existing owner, add a row to its table or corpus before adding a function, and keep one authoritative owner per claim.

- Several assertions can establish one outcome; a named table covers related inputs with useful diagnostics. Split unrelated scenarios.
- A behavior-preserving refactor relies on existing contract tests. Do not bind tests to newly extracted private helpers unless they own an otherwise unprotected invariant.
- Real components where the boundary matters (file-backed SQLite, real temp directories, a fresh process for exit codes); doubles only at an intentional seam, and the double must fail on missing, duplicate or extra calls.
- Compare error variants for classification; compare exact text only where text is the contract (public output, JSONL). Snapshot tests suit stable reviewable output; normalize only declared volatile fields.
- Deterministic: no wall sleeps, no retries, no random order, no shared process environment (edition 2024 makes `set_var` `unsafe` for this reason). Concurrency is coordinated with explicit gates (`oneshot`, `Notify`, `Barrier`) and paused Tokio time (`#[tokio::test(start_paused = true)]`, `time::advance`). A deadline bounds a hang; it is never the synchronization.
- `#[track_caller]` on shared assertion helpers so failures point at the row, not the helper. Failure output names scenario, expected and observed, and never dumps secrets or raw hostile bytes.
- Property tests (`proptest`: strategies, shrinking, a persisted regression seed) earn their place when a parser or state space outgrows representative rows. Fuzzing, model checking and snapshot frameworks follow the same bar: a named boundary that readable tables no longer cover.

### Test support in the crate

- Unit tests live in `#[cfg(test)] mod tests` beside the code (private access through `use super::*`); integration tests in `tests/` see only the public surface, each file its own crate, shared helpers under `tests/common/`.
- Hooks a test needs on a production type (fault injection, gates, scripted peers) go behind a Cargo feature, conventionally `test-support`, enabled for the crate's own tests through `[dev-dependencies] crate = { path = ".", features = ["test-support"] }`. `#[cfg(test)]` on a production type is invisible to integration tests and other crates, and it makes the type's shape differ between `cargo build` and `cargo test`. Keep the feature additive and the hooks few: one fault plan, not one method per fault.
- A fake that mirrors a renderer (an inverse parser) lives next to the renderer with a round-trip test, so a format change fails there rather than in a distant journey.

## Use the project's tooling

Read the toolchain, Cargo configuration and verification instructions before choosing commands. Check supported feature combinations rather than assuming `--all-features`. Preserve the declared `rust-version` and lockfile; a lint or an example never justifies upgrading a compiler or dependency. Missing tooling is reported, not worked around silently.

### Clippy

CI runs Clippy with `-D warnings`, so every warn-level lint is a hard failure. Lint levels belong in `[lints.rust]`/`[lints.clippy]` in `Cargo.toml` (groups with `priority = -1` so individual lints override them) and thresholds in `clippy.toml`; command-line flags drift.

- Fix the problem the lint names. Suppress with `#[expect(clippy::lint, reason = "...")]` only when the design is intentional; `#[allow]` only when the lint's firing varies by platform or configuration (`allow_attributes_without_reason` catches a bare one).
- Group semantics on the pinned toolchain: `style`/`complexity`/`perf`/`suspicious`/`correctness` are on by default; `pedantic` and `nursery` are opt-in groups; `restriction` is never enabled as a group, only lint by lint. Placements move between releases (`uninlined_format_args` is pedantic again since 1.89, `cognitive_complexity` is restriction since 1.91, `module_name_repetitions` is restriction since 1.84), so a new allowance is checked against the pinned Clippy's lint list, not memory.
- The lints that encode rules in this skill: `cast_*` (narrowing), `unwrap_used` with `allow-unwrap-in-tests = true` (panics are proofs), `await_holding_lock` (guards across `.await`, warn by default), `too_many_arguments`/`too_many_lines` (extraction prompts), `undocumented_unsafe_blocks` (`// SAFETY:`), `result_large_err` (boxed payloads), `redundant_clone` (nursery, worth enabling alone), `dbg_macro`/`todo`/`unimplemented` (never in a merge).

## Measure performance claims

Start with the workload and a baseline in an optimized profile. Look at algorithmic work, I/O, allocation, contention and data layout before a cosmetic rewrite. An obviously unnecessary copy can go without a benchmark; claiming a speedup needs one, under consistent conditions, with variance and scope stated. Inlining, boxing, generics, iterator chains and shared ownership have context-dependent costs; a microbenchmark win may not move the end-to-end number.

## Own concurrent work

Every task, channel and shared value has an owner, a lifetime, a capacity and a failure behavior you can point at. Adding `async` or a task is not an improvement by itself.

- **Bounded channels with a named full policy.** `mpsc::channel(n)`: `send().await` applies backpressure; `try_send` returns `Full`/`Closed` for a drop-newest policy; a ring buffer or `broadcast` (which reports `Lagged(n)`) for drop-oldest; `watch` coalesces to the latest value. `unbounded_channel` only when the producer is already bounded by something you can name, and say what. The policy is part of the contract, not an implementation detail.
- **Spawned tasks are owned.** A dropped `JoinHandle` detaches the task; it keeps running. Hold handles in a `JoinSet` or a struct whose `Drop` aborts them, join or abort on every exit path (success, error, cancellation), and treat `JoinError::is_panic()` as a failure to report. `abort()` cannot stop a `spawn_blocking` task that has started.
- **Concurrency has a limit.** A `Semaphore` or a `JoinSet` with a cap bounds parallel work; "spawn one per item" is unbounded.
- **Shared state.** A `std::sync::Mutex` is right for short critical sections that do not await; contention blocks the worker thread, so a hot lock becomes a dedicated owner task with messages, or sharded state. `tokio::sync::Mutex` only when the guard must live across `.await`, which is rare and slower. Atomics for counters and flags; `Mutex<bool>` is a smell (`mutex_atomic`, `mutex_integer`).

### Async rules

- **Nothing blocks on a worker thread.** `std::fs`, `std::net`, `Command::output()`, a sync `Mutex` under contention, CPU-heavy loops: wrap in `spawn_blocking` (short-lived, bounded; pool is large by default and queues beyond it) or move to a dedicated `std::thread` for long-lived work. `spawn_blocking` work cannot be cancelled; bound it with its own deadline and an owner that reaps it.
- **No `std::sync::MutexGuard` or `RefCell` borrow across `.await`.** The guard makes the future `!Send` (spawn fails to compile) and, on a single-threaded runtime, deadlocks. Scope the lock in a block or in a sync method that returns the value (`await_holding_lock`, `await_holding_refcell_ref`).
- **`select!` branches are cancel-safe or own their progress.** Cancel-safe: `mpsc`/`broadcast` `recv`, `watch::changed`, `accept`, `signal.recv`, `AsyncReadExt::read`, `AsyncWriteExt::write`, `StreamExt::next`. Not cancel-safe: `read_exact`/`read_to_end`/`read_to_string`/`write_all` (partial data lost) and `Mutex::lock`/`Semaphore::acquire`/`Notify::notified` (queue place lost). A non-cancel-safe future in a loop is pinned outside the loop and passed as `&mut`. `biased;` makes polling order explicit (shutdown first); default order is random.
- **Cancellation is dropping the future.** Completed side effects remain; a dropped Tokio future runs no code afterwards. A value that must be settled on cancellation is a guard with `Drop`, not a line after the `.await`. Use the project's cancellation type rather than adding `tokio-util`.
- **`async fn` in traits** (Rust 1.75) for traits used only generically; `#[async_trait]` (boxes each future, one allocation per call) for a trait used as `dyn Trait` or whose futures must be `Send` from generic code. Arany's `Provider` is `Arc<dyn Provider>`, so it stays on `async-trait`.
- **Deadlines wrap awaits**: `tokio::time::timeout(d, fut)` at the boundary that owns the budget, never a sleep-and-poll. Paused test time auto-advances when the runtime is idle, so pause only after real I/O finishes.
- An `async fn` with no `.await` is a sync function (`unused_async`); a future over 16 KiB on the stack (`large_futures`) is boxed at its spawn site.

Safe Rust prevents data races; it does not prove freedom from deadlocks, logic races or leaked tasks. Those are tested with gates and asserted cleanup.

## Unsafe code requires a proof

Prefer the safe operation that expresses the requirement. When `unsafe` is justified and authorized:

- One safe wrapper per unsafe capability; the `unsafe` block is as small as the operation. Edition 2024 warns on unsafe operations in an `unsafe fn` without an inner block (`unsafe_op_in_unsafe_fn`): write the block and its reason.
- `// SAFETY:` before every block states each obligation and why it holds here: pointer validity and provenance, bounds, alignment, initialization, aliasing, lifetime, thread behavior, FFI ownership of the buffer or handle. A non-null pointer proves almost nothing.
- Unsafety is non-local: safe code in the same module can break the invariant (the Rustonomicon's `Vec` `cap` example). Keep the fields the invariant depends on private so only the module that upholds it can write them, and keep that module small (Rust Design Patterns "contain unsafety in small modules").
- An `unsafe fn` is one whose *caller* must uphold an obligation; its signature says so and `missing_safety_doc` applies when it is `pub`. `unsafe impl Send/Sync` is the same contract about threads.
- Evidence: focused review, a test per obligation and Miri where the code is pure Rust. Passing tests do not establish soundness for every execution; FFI and OS calls need the vendor's contract read, not guessed.

Sources: [Book ch. 11 test organization](https://doc.rust-lang.org/book/ch11-03-test-organization.html), [Cargo features](https://doc.rust-lang.org/cargo/reference/features.html), [Cargo `[lints]`](https://doc.rust-lang.org/cargo/reference/manifest.html#the-lints-section), [Clippy lint configuration](https://doc.rust-lang.org/clippy/lint_configuration.html), [Clippy changelog](https://github.com/rust-lang/rust-clippy/blob/master/CHANGELOG.md), [`#[expect]` and reasons](https://blog.rust-lang.org/2024/09/05/Rust-1.81.0/), [proptest](https://docs.rs/proptest/latest/proptest/), [Tokio shared state](https://tokio.rs/tokio/tutorial/shared-state), [Tokio `select!` cancellation safety](https://docs.rs/tokio/latest/tokio/macro.select.html#cancellation-safety), [`spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html), [`mpsc::channel`](https://docs.rs/tokio/latest/tokio/sync/mpsc/fn.channel.html), [`JoinHandle`](https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html), [`time::pause`](https://docs.rs/tokio/latest/tokio/time/fn.pause.html), [async-trait](https://docs.rs/async-trait/latest/async_trait/), [Reference dyn compatibility](https://doc.rust-lang.org/reference/items/traits.html#dyn-compatibility), [Rustonomicon working with unsafe](https://doc.rust-lang.org/nomicon/working-with-unsafe.html), [Miri](https://github.com/rust-lang/miri), [Performance Book](https://nnethercote.github.io/perf-book/), [Effective Rust items 16, 17, 29, 30](https://www.lurklurk.org/effective-rust/).
