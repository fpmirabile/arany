# Verification and concurrency

## Test a claim

Choose a test for the observable behavior or failure it protects. Reuse the existing test owner where possible. A test can use several assertions to establish one outcome; a named table can cover related input cases with useful diagnostics. Split unrelated scenarios when that improves diagnosis. Neither assertion count nor coverage percentage establishes quality.

For a behavior-preserving refactor, existing contract tests are usually the right evidence. Avoid binding tests to newly extracted private helpers unless they own an otherwise unprotected meaningful invariant. Use real components where boundary behavior matters and doubles where the project has an intentional seam. Share setup or assertion machinery when it improves diagnostics while keeping the scenario understandable.

Snapshot tests suit stable, reviewable output contracts. Inspect intentional changes and normalize only declared volatile fields. Plain assertions are often clearer for small values. Compare error variants for classification; assert exact text when text is itself a contract. Test the actual failure boundary: an in-memory success cannot establish disk durability.

When concurrency is relevant, coordinate with explicit gates or a controlled clock and verify cleanup as well as output. Test deadlines should bound hangs, not substitute for synchronization. Property or fuzz tests can help when a parser or state space outgrows representative examples; new frameworks need a concrete benefit.

## Use the project's tooling

Read the toolchain, Cargo configuration, and local verification instructions before choosing commands. Run the applicable format, lint, and test checks; compile documentation examples when changing their API usage. With no local commands, typical starting points are `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`, adding workspace or feature flags for the actual project.

Check supported feature combinations rather than assuming `--all-features` is valid. Preserve the declared MSRV and lockfile policy. A lint or example does not justify updating the compiler or dependencies. If tooling is missing, report it and use available evidence rather than silently changing the environment.

Interpret Clippy diagnostics in context. Fix actual problems; use a narrow, reasoned exception when the design is intentional and project policy permits it. `#[expect]` is useful when a lint is expected to fire on the supported compiler/configuration; an `allow` may be appropriate where that expectation is unstable. Broad lint groups such as `pedantic` are deliberate team choices, not automatic upgrades.

## Measure performance claims

Start with the relevant workload and a baseline in a representative optimized profile. Investigate algorithmic work, I/O, allocation, contention, and data layout before a cosmetic rewrite. An obvious unnecessary copy can be simplified without starting a benchmark project; claiming a speedup needs evidence.

Compare under consistent conditions, account for variance, and state what the measurement covers. Inlining, boxing, generics, iterator chains, and shared ownership have context-dependent costs. A microbenchmark win may not improve end-to-end behavior. Choose a change whose benefit justifies its readability and maintenance cost.

## Own concurrent work

Keep the owner, lifetime, capacity, and failure behavior of tasks, channels, and shared state explicit. A bounded queue still needs a policy for a full queue. Apply project resource budgets and cancellation contracts; adding async or a task is not inherently an improvement.

For Tokio, a standard mutex can be suitable for short, low-contention critical sections that finish before awaiting. Contention can still block an executor thread. An async mutex supports waiting without blocking the thread and, when necessary, guards held across awaits, but does not eliminate deadlock or long-held-lock problems. A dedicated owner with messages may better fit a stateful I/O resource. Choose from actual ownership and scheduling needs.

Dropping a future can cancel its remaining work, but completed side effects remain. A dropped Tokio `JoinHandle` detaches its task; it does not stop it. Check the cancellation guarantees of operations used in `select!`, partial progress, who joins or aborts spawned tasks, and what cleanup is required. Blocking work needs its own lifecycle analysis. Safe Rust prevents many memory errors; it does not prove freedom from deadlocks, races in business logic, or resource leaks.

## Unsafe code requires a proof

Prefer safe operations that express the requirement. When unsafe is justified and authorized, isolate it behind a safe boundary and state the exact obligations for each operation: allocation validity, bounds, alignment, initialization, aliasing, lifetimes, and thread behavior as applicable. A non-null pointer alone proves very little. Safe callers must not be able to violate the invariant. Use focused review and tools such as Miri when applicable; passing tests does not establish soundness for every execution.

Sources: [Clippy usage](https://doc.rust-lang.org/clippy/usage.html), [Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html), [Tokio shared state](https://tokio.rs/tokio/tutorial/shared-state), [Tokio select](https://tokio.rs/tokio/tutorial/select), [JoinHandle](https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html), [Rust unsafe obligations](https://doc.rust-lang.org/reference/unsafe-keyword.html), and [Miri](https://github.com/rust-lang/miri).
