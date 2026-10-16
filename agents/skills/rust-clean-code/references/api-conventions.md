# API conventions

The Rust API Guidelines checklist ids (`C-…`) are cited so a review can name the rule. Internal code follows them too: a `pub(crate)` item is read by the same people, and conventions are what make a name predictable without opening its definition.

## Naming (C-CASE, C-CONV, C-GETTER, C-ITER, C-CTOR, C-WORD-ORDER)

| Item | Convention |
| --- | --- |
| Modules, functions, methods, locals, macros | `snake_case`; acronyms lowercased (`parse_url`) |
| Types, traits, variants, type parameters | `UpperCamelCase`; acronyms as one word (`Uuid`, `HttpClient`) |
| Constants, statics | `SCREAMING_SNAKE_CASE` |
| Lifetimes | short, lowercase (`'a`, `'src`) |
| Modules | a noun for what the module owns (`ledger`, `catalog`); never `util`, `helpers`, `common`, `misc` |
| Error types | verb-object-error order like std (`ParseAddrError`), consistent within the crate |

Conversions encode cost and ownership in the prefix:

| Prefix | Cost | Ownership | Example |
| --- | --- | --- | --- |
| `as_` | free | borrowed → borrowed | `as_str`, `as_mut_slice` |
| `to_` | expensive | borrowed → borrowed / owned; `Copy` owned → owned | `to_string`, `to_vec`, `to_ascii_uppercase` |
| `into_` | variable | owned → owned (consumes) | `into_inner`, `into_iter` |

- `mut` appears where it appears in the type: `as_mut_slice`, not `as_slice_mut`.
- Getters carry the field name alone: `first()` and `first_mut()`, never `get_first()`. `get` is reserved for one obvious lookup (`Cell::get`, `HashMap::get`).
- Collections expose `iter` (`&self`), `iter_mut` (`&mut self`) and `into_iter` (`self`); their iterator types are `Iter`, `IterMut`, `IntoIter`.
- Constructors are associated functions: `new` (with or without arguments), `with_<detail>` for a secondary constructor, `from_<source>` for a conversion that needs extra arguments or can fail or be `unsafe`; I/O resources use their verb (`open`, `connect`, `bind`). `new()` with no arguments and `Default::default()` behave identically.
- Predicates read as questions: `is_empty`, `has_children`, `can_retry`. A `bool` parameter that changes a mode at the call site becomes an enum (C-CUSTOM-TYPE).
- Features avoid filler: `serde`, `std`, `test-support`; never `use-serde` or `no-std` (C-FEATURE). Features are additive ([Cargo features](https://doc.rust-lang.org/cargo/reference/features.html)).

## Derives and standard traits (C-COMMON-TRAITS, C-DEBUG, C-SERDE, C-SEND-SYNC, C-STRUCT-BOUNDS)

Derive eagerly: the orphan rule stops anyone else from adding these later, and a missing `Debug` surfaces as an unusable `assert_eq!` or `{:?}` in a test.

| Type | Derive |
| --- | --- |
| Every public data type | `Debug` |
| Value types (ids, configs, small records) | `Clone, Debug, PartialEq, Eq`; add `Hash` when used as a key, `PartialOrd, Ord` when ordering is meaningful, `Default` when an empty value is meaningful |
| Small plain data (ids, enums without payload, `Duration`-like) | `Copy` too; this is an API commitment, since removing `Copy` later breaks callers |
| Journal and wire types | `Serialize, Deserialize`; `#[serde(deny_unknown_fields)]` on inputs that must reject unknown shapes; explicit `rename_all` where the wire format is a contract |
| Error types | `Debug` plus `Display`/`Error` (`thiserror`); `Clone`/`PartialEq` only when tests or callers compare them, since many sources (`io::Error`) are not `Clone` |

- `PartialEq` without `Eq` only when a field is a float or otherwise lacks total equality. `Hash` must agree with `Eq`.
- No trait bounds on struct definitions for derivable traits (`struct Wrapper<T: Clone>`): bounds belong on `impl` blocks; derive handles the rest and adding a bound later is a breaking change.
- `Send` and `Sync` are auto-derived; a type that stops being `Send` because of an `Rc`, a raw pointer or a `std::sync::MutexGuard` held in a field is a design signal. Assert the bound in a test (`fn assert_send<T: Send>() {}`) when a type crosses a thread boundary by contract.
- Conversions implement `From`/`TryFrom`, `AsRef`/`AsMut`; never `Into`/`TryInto` directly (the blanket impls derive them). Define a conversion on the more specific type (`str::as_bytes`, not a method on `[u8]`).
- Collections implement `FromIterator` and `Extend` so `collect` and `extend` work (C-COLLECT).
- `Deref`/`DerefMut` only on smart pointers; using them to fake inheritance hides method resolution (C-DEREF).

## Visibility and module layout

- Items are private by default; widen to `pub(super)`, then `pub(crate)`, then `pub` only when a caller outside that scope exists. `pub` through the crate root is a commitment to tests and binaries; re-export only what they use.
- One concept per module, named for the thing it owns. A module file `name.rs` holds the interface; submodules live in `name/` (`name/parse.rs`), not `name/mod.rs`. Tests for a module go in `#[cfg(test)] mod tests` at the bottom or, when they outgrow it, a sibling `tests.rs` declared as `#[cfg(test)] mod tests;`.
- A module grows a new file when it gains a second responsibility with its own invariants, not when it crosses a line count. Splitting a cohesive state machine across helpers makes the reader chase state; keep it whole and say so.
- Deep modules: a small `pub(crate)` surface over a substantial private implementation beats many tiny public types. Hide the helper, expose the operation.
- Imports: full paths for types and traits (`use std::collections::HashMap;`), the parent module for functions (`fs::read_to_string`). Glob imports only for `use super::*;` in tests and a crate's own prelude. Keep `use` at the top of the file; `use` inside a function only for a macro-scoped or platform-gated import.
- `std` paths over `core`/`alloc` in a `std` crate; `std::sync::Mutex` versus `tokio::sync::Mutex` is spelled out at the use site when both are in scope.

## Attributes that change the contract

- **`#[must_use]`** on a function or type whose discarded result is a bug: builders, guards and handles, reservations that must be settled, pure computations (`fn checksum(&self) -> u64`). Add a message when the reason is not obvious: `#[must_use = "the lease is released on drop"]`. `Result` and iterators already carry it. Do not add it to every getter; Clippy's `must_use_candidate` is pedantic noise, the hand-placed ones are contracts.
- **`#[non_exhaustive]`** on a `pub` enum or struct that downstream crates match or construct and that will gain variants or fields. It has no effect inside the defining crate, so in a single internal crate it buys nothing; apply it when a type crosses a crate boundary.
- **`#[expect(lint, reason = "...")]`** over `#[allow]` for a deliberate exception: the compiler warns when the exception stops being needed. `#[allow]` only where the lint firing is itself unstable across platforms or configurations.
- **`#[track_caller]`** on assertion helpers and `expect`-style wrappers so a panic points at the caller.
- **`#[inline]`** only with a measurement; cross-crate inlining of small generic functions already happens. **`#[cold]`** on error-construction paths is cheap and rarely wrong.
- **`#[derive(...)]`** order: `Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash` then third-party (`Serialize, Deserialize`), so diffs stay stable.

## Comments and documentation

Comments default to none. Names, types and tests carry the intent; behavior and limits live in the owning spec, not beside the code.

- One short `//` comment where the reason is non-obvious: a workaround for a platform bug, an ordering constraint, a `ponytail:` ceiling. The comment states *why*, never what the next line does.
- `// SAFETY:` before every `unsafe` block, stating the obligations that make it sound ([unsafe](verification-and-concurrency.md#unsafe-code-requires-a-proof)).
- No `///` or `//!` doc comments and no multi-line comment blocks in internal code. An implementation invariant is pinned by a test; if its reason is non-obvious, one short `//` line is the limit. `// SAFETY:` is the one exception: a single line stating the obligations, spilling to a second `//` line only when a distinct obligation needs it. Behavior and limits live in [`docs/specs/`](../../../../docs/specs/README.md).
- Published-library rustdoc only, not for this internal crate: `# Errors`, `# Panics`, `# Safety` and `# Examples` sections (C-FAILURE, C-EXAMPLE), a one-sentence summary line and intra-doc links such as `` [`Type`] ``. Keep Clippy's `missing_errors_doc`/`missing_panics_doc` allowed here. If a crate is split out and published, document its `pub` surface with these then.

Sources: [API Guidelines naming](https://rust-lang.github.io/api-guidelines/naming.html), [interoperability](https://rust-lang.github.io/api-guidelines/interoperability.html), [predictability](https://rust-lang.github.io/api-guidelines/predictability.html), [future proofing](https://rust-lang.github.io/api-guidelines/future-proofing.html), [Book ch. 7 modules](https://doc.rust-lang.org/book/ch07-00-managing-growing-projects-with-packages-crates-and-modules.html), [`must_use` and lint attributes](https://doc.rust-lang.org/reference/attributes/diagnostics.html), [`non_exhaustive`](https://doc.rust-lang.org/reference/attributes/type_system.html), [rustdoc conventions](https://doc.rust-lang.org/rustdoc/how-to-write-documentation.html), [Effective Rust items 10, 22, 23, 27](https://www.lurklurk.org/effective-rust/).
