# Review checklist

Apply to the diff, not the whole file. Each line is a yes/no check; a "no" is a finding only when you can name the consequence and an alternative. Report findings by severity: correctness requirement, then project convention, then design preference.

## Types and API

- [ ] New `pub` item has a caller outside its module; otherwise it is `pub(crate)`/`pub(super)`/private.
- [ ] Names follow the conventions: `as_`/`to_`/`into_` by cost, getters without `get_`, `new`/`with_`/`from_` constructors, noun module names.
- [ ] Data types derive `Debug`; value types derive `Clone, PartialEq, Eq` (+`Hash`/`Ord`/`Default` where meaningful); journal/wire types derive `Serialize, Deserialize` with the agreed serde attributes.
- [ ] A `bool` parameter or a pair of booleans that encode a mode is an enum.
- [ ] Invalid combinations are unrepresentable (enum over flags, newtype over raw id, private fields with a validating constructor).
- [ ] A new trait has two implementations or one plus an intentional test seam; a closed set is an enum.
- [ ] `#[must_use]` on a returned guard, reservation, handle or builder whose discard is a bug.
- [ ] Conversions implement `From`/`TryFrom`, not `Into`.

## Ownership and allocation

- [ ] Read-only parameters are `&str`/`&[T]`/`&Path`; stored values are taken by value.
- [ ] Each `clone()` is cheap, rare or decouples a lifetime; `mem::take`/`Option::take` where a move out of `&mut` was needed.
- [ ] No `collect` followed by a single iteration; no `format!` where `write!` into a buffer or a literal would do; capacity reserved where the size is known.
- [ ] `Arc`/`Rc` appears because ownership is shared, not to silence a borrow error.

## Errors and panics

- [ ] Failures return `Result` with a variant the caller can match; no `Result<_, String>`, no `to_string()` on an error below the presentation boundary.
- [ ] `source()` carries the cause; `Display` is one lowercase sentence without the cause.
- [ ] Context is attached once, at the layer that knows the operation; no log-and-return at every layer.
- [ ] Every `expect` has a proof in its message; no `unwrap` outside tests; no bare `unreachable!()`.
- [ ] Error text and `Debug` output contain no secrets, hostile control bytes or untrusted content unescaped.

## Casts and arithmetic

- [ ] Narrowing and sign changes use `try_from`/`try_into` (or a named saturation on a declared bound); `as` only widens.
- [ ] Arithmetic on externally supplied numbers is checked or saturating with a named bound.

## Bounds and resources

- [ ] Every new queue, buffer, cache, request body, output capture, retry loop and concurrency fan-out has a limit and a stated overflow policy.
- [ ] Every spawned task has an owner that joins or aborts it on all exit paths; no detached `JoinHandle`.
- [ ] Every blocking call in async context is in `spawn_blocking` or a dedicated thread, with its own deadline.

## Async

- [ ] No `std::sync::MutexGuard` or `RefCell` borrow lives across an `.await`.
- [ ] `select!` branches are cancel-safe or the non-safe future is pinned outside the loop; shutdown branches come first under `biased;` when order matters.
- [ ] A value that must be settled on cancellation is a `Drop` guard, not code after the `.await`.
- [ ] `async-trait` only where the trait is `dyn`; native `async fn` for generic-only traits.
- [ ] Timeouts wrap the await at the boundary that owns the budget; no sleep-and-poll.

## Unsafe

- [ ] Each `unsafe` block is minimal, preceded by `// SAFETY:` naming every obligation, and wrapped in a safe interface whose callers cannot break the invariant.
- [ ] Fields the invariant depends on are private to the module that upholds it.
- [ ] Platform or FFI calls have the vendor contract cited and a test per obligation.

## Tests

- [ ] Each new test names the claim and the defect it catches; an existing owner (table, corpus, journey) was extended first.
- [ ] No wall sleeps, retries, random order or shared process environment; concurrency uses gates and paused time.
- [ ] Error paths assert the variant; output contracts assert exact bytes; durability asserts a reopened store, not memory.
- [ ] Test-only hooks on production types sit behind the `test-support` feature, not `#[cfg(test)]`.

## Lints and comments

- [ ] Clippy clean under the project's configuration; every suppression is `#[expect(..., reason = "...")]` with an intentional design behind it.
- [ ] A function over Clippy's length or argument threshold is a declared state machine or was split.
- [ ] No new comments that narrate code; at most one short `//` where the reason is non-obvious; no `///` or `//!` doc comments in internal code, per the repository rule.
- [ ] No `dbg!`, `todo!`, `unimplemented!`, `println!` debugging, or commented-out code.
