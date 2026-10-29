# Design and errors

## Functions and boundaries

Extract a function when its name explains a meaningful operation, isolates a tricky invariant or centralizes a decision that must change consistently. A single caller can justify a helper; similar-looking expressions can stay separate when they are independent decisions.

Thresholds are prompts, and Clippy enforces two of them: `too_many_arguments` (fires above 7 parameters, warn by default) and pedantic `too_many_lines` (100). A function over either is a cohesive state machine or transition table, in which case say so in `#[expect(clippy::too_many_lines, reason = "one transition table")]`, or it is split. Seven parameters usually means a missing struct: group the ones that travel together.

Keep a cohesive flow together when splitting makes the reader chase state through helpers. Local mutation is often the clearest algorithm; effects at meaningful boundaries help tests, but do not wrap every operation in a trait or layer.

## Control-flow idioms

Prefer the form that puts the happy path at the left margin:

- `let Some(x) = opt else { return Err(..) };` for a required value with an early exit; it replaces a `match` whose other arm only diverges.
- `if let Some(a) = x && a.ready() && let Ok(b) = parse(a)` (edition 2024, Rust 1.88) replaces nested `if let` pyramids. Each `let` binds for the rest of the chain.
- `matches!(state, State::Open | State::Draining)` for a boolean over patterns; `match` when each arm does work.
- `Option`/`Result` combinators (`map`, `and_then`, `ok_or_else`, `unwrap_or_default`, `is_some_and`) for one or two steps; a `match` when the branches carry logic or need naming. `?` applies to `Option` inside a function returning `Option`.
- Exhaustive `match` without a `_` arm on the crate's own enums, so adding a variant fails to compile at every site that must decide. Use `_` only for foreign `#[non_exhaustive]` types.
- Iterate with `for` when the body mutates or branches; with iterator chains when it is a transformation ([ownership](ownership.md#copy-cow-and-iteration)).

## Concrete types, enums, and traits

Start with the shape the problem has and move one step only when the current one cannot express it:

- **Concrete type:** one implementation with no current need for substitution.
- **Enum:** a known set of alternatives; exhaustive matching keeps every case visible. A closed set of implementations (sites, record kinds, providers known at compile time) is an enum with a `match`, not a trait.
- **Generic (`T: Trait`, argument-position `impl Trait`):** callers supply a concrete type and code shares a behavior contract; monomorphization helps optimization and costs compile time and code size. Return-position `impl Trait` hides a concrete type the implementation chooses; in edition 2024 it captures all in-scope lifetimes unless narrowed with `use<..>`.
- **`dyn Trait`:** erase the type for runtime selection, mixed implementations or to stop generics spreading through a codebase. `&dyn` borrows, `Box<dyn>`/`Arc<dyn>` own. The indirection cost is real only on a measured hot path.

A trait describes a capability or a substitution boundary with at least two implementations, or one plus a test double at an intentional seam. A trait per struct, an inheritance-style hierarchy or generics everywhere do not follow from "separation of concerns". Dyn compatibility is the Reference's list: no generic methods, no `Self` outside the receiver, no `async fn`/`impl Trait` returns unless `where Self: Sized`, no associated consts. An `async fn` in a trait that is used as `dyn` needs `async-trait` or a boxed future ([async rules](verification-and-concurrency.md#async-rules)).

## Model useful guarantees

Private fields with a validating constructor, or a newtype (`struct Port(u16)`, `struct SessionId(Uuid)`), establish an invariant once (C-NEWTYPE, C-VALIDATE, C-STRUCT-PRIVATE). An enum replaces flag combinations that admit invalid states (`struct { running: bool, finished: bool }` becomes `enum Status`). Add structure when it removes realistic misuse or repeated validation.

Typestate (lifecycle stages as distinct types) fits a protocol whose legal sequence is known in the calling code. A runtime state machine fits an enum and validated transitions better, especially when loading persisted state or processing external events. A state marker cannot prove an external resource is still there; keep that failure handling. Prefer each state carrying exactly its data over a marker beside contradictory `Option` fields.

A builder (C-BUILDER) is for a type with several optional parts; a struct literal with `..Default::default()` or two `with_` constructors covers most cases without one.

## Error mechanics

Use `Option` for meaningful absence, `Result` for failure the caller handles, `Result<Option<T>, E>` when both are distinct outcomes. Propagate with `?` when propagation is the behavior; `match` when the caller recovers or classifies.

A good error type (C-GOOD-ERR) implements `std::error::Error + Send + Sync + 'static`, so it can cross threads and be boxed. In this crate:

```rust
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("state storage is full")]
    StorageFull,
    #[error("session {0} has an active operation")]
    SessionBusy(SessionId),
    #[error("could not open journal at {path}")]
    Open { path: PathBuf, #[source] source: std::io::Error },
}
```

- **Variants classify, prose does not.** A caller matches `StoreError::StorageFull`; nobody matches a string. A variant carries the data needed to act or report (`path`, id, limit), not a pre-rendered message.
- **`Display` is one lowercase sentence without trailing punctuation and without the source.** `source()` returns the cause; a report walks the chain. Rendering the cause in `Display` *and* returning it from `source()` prints it twice.
- **`#[from]` only when the mapping is one-to-one and needs no context.** `io::Error` into a variant with a `path` needs `.map_err(|source| StoreError::Open { path, source })?`, not `#[from]`. `#[error(transparent)]` wraps a cause without adding a layer of text.
- **Attach context once**, at the layer that knows the operation, with `map_err` or a typed variant. Report at the presentation boundary. Logging and returning the same error at each layer prints it N times.
- **Never `e.to_string()` before the presentation boundary.** `map_err(|e| e.to_string())` or `Result<T, String>` loses the variant and the chain; the terminal and the JSONL writer render, nothing below them does.
- **`Box<dyn Error + Send + Sync>` or `anyhow` only in a binary's `main`/top-level reporting**, where no caller matches. A library-style module keeps a typed enum. Do not add `anyhow` to a crate that already standardizes on typed errors.
- **Keep error values small.** Clippy's `result_large_err` fires above 128 bytes; box a large payload rather than widening every `Result`.
- Error text is output: it must not leak secrets, hostile control bytes or untrusted content unescaped.

## Integer and float casts

`as` never fails and therefore never tells you about a problem: integer narrowing truncates, same-width sign changes reinterpret bits, float-to-int saturates and maps NaN to 0. Pedantic Clippy names each case (`cast_possible_truncation`, `cast_sign_loss`, `cast_possible_wrap`, `cast_precision_loss`, `cast_lossless`).

| Conversion | Write |
| --- | --- |
| Widening or lossless (`u8` → `u32`, `u32` → `u64`, `u32` → `f64`) | `u64::from(n)` or `.into()`; `as` is acceptable but `From` documents losslessness |
| Narrowing or sign change (`u64` → `u32`, `i64` → `usize`, `usize` → `u32`) | `u32::try_from(n)?` or `.try_into()`; on a declared bound, `u32::try_from(n).unwrap_or(u32::MAX)` with the saturation named |
| Float to integer | `as` only when saturation and NaN → 0 are the intended policy; otherwise check range first |
| Index arithmetic | stay in `usize`; convert the external value once at the boundary |
| Enum discriminant | `as` is fine for a fieldless enum; `TryFrom<u8>` for the inverse |

Checked, wrapping and saturating arithmetic (`checked_add`, `saturating_sub`) are the same decision for operations: a bound you can name picks the method.

## Panics and proofs

Expected malformed input, unavailable resources, network and storage failures return `Result`. A panic is for a violated internal invariant, and the message is the proof: `expect("ids were validated by admit()")`. `unwrap()` has no proof, so it belongs in tests only. `unreachable!()` with no argument is `unwrap()` in a hat; give it the reason. A `std::sync::Mutex` poisoned by a panic elsewhere is handled explicitly: `lock().unwrap_or_else(PoisonError::into_inner)` when the data stays valid, `expect` when poisoning is itself a bug.

Assess the panic's blast radius: in a Tokio task it ends the task and surfaces as `JoinError::is_panic()`; in the Guard helper it ends the process. A harmless-looking default (`unwrap_or_default()`) can hide a real defect; substitute a default only when that is the contract.

Sources: [API Guidelines checklist](https://rust-lang.github.io/api-guidelines/checklist.html), [Reference dyn compatibility](https://doc.rust-lang.org/reference/items/traits.html#dyn-compatibility), [Reference numeric casts](https://doc.rust-lang.org/reference/expressions/operator-expr.html#type-cast-expressions), [`std::error::Error`](https://doc.rust-lang.org/std/error/trait.Error.html), [thiserror](https://docs.rs/thiserror/latest/thiserror/), [Book ch. 9 panic or Result](https://doc.rust-lang.org/book/ch09-03-to-panic-or-not-to-panic.html), [Rust 1.88 let chains](https://blog.rust-lang.org/2025/06/26/Rust-1.88.0/), [Rust 2024 edition guide](https://doc.rust-lang.org/edition-guide/rust-2024/index.html), [Effective Rust items 1–7, 12, 18](https://www.lurklurk.org/effective-rust/).
