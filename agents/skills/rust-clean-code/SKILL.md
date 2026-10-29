---
name: rust-clean-code
description: Guide Rust implementation, refactoring, and review with contextual advice on ownership, errors, API design, testing, and concurrency, explaining the meaningful tradeoffs.
license: MIT
---

# Rust Clean Code

Write Rust whose intent, ownership and failure modes are visible at the call site. This is engineering judgment, not a rewrite mandate: a sound function left alone is a valid outcome. The guidance adapts Apollo GraphQL's Rust Best Practices and is checked against the Rust API Guidelines, the Reference, Clippy and Tokio documentation; see [attribution and sources](references/attribution.md).

## Apply in context

Read the repository instructions and the nearby code first. Use the declared edition, `rust-version`, dependencies and verification commands. Repository rules win over this skill's preferences.

Separate three kinds of advice and say which one you are giving:

- **Correctness requirement:** a language safety obligation or a product contract. Name the concrete failure; never trade it for brevity.
- **Project convention:** agreed architecture, style and checks. Follow the authoritative local source.
- **Design preference:** borrowing, extraction, dispatch and similar choices. Weigh clarity, coupling and resource cost for this use; a justified exception is ordinary engineering.

For a change, name the behavior to preserve and the actual source of complexity, then make the smallest change that removes it. A review finding states a concrete consequence and a usable alternative; taste alone is an optional observation, not a defect.

## Decision rules

The defaults that settle most choices. Each links to the reference that holds the reasoning and the exceptions.

1. **Make illegal states unrepresentable.** An enum over flag combinations, a newtype with a validating constructor, private fields. [Design](references/design-and-errors.md#model-useful-guarantees)
2. **Borrow to read, own to keep.** `&str`/`&[T]` parameters for reads, owned values for stored data, a clone only to decouple lifetimes when it is cheap or rare. [Ownership](references/ownership.md)
3. **Concrete type, then enum, then generic, then `dyn`.** Reach for the next step only when the current one cannot express the substitution. [Design](references/design-and-errors.md#concrete-types-enums-and-traits)
4. **Errors classify; they do not narrate.** Typed error enums (`thiserror`), `source()` for the cause, context attached once at the layer that knows the operation, and no `to_string()` before the presentation boundary. [Design](references/design-and-errors.md#error-mechanics)
5. **Narrowing goes through `TryFrom`.** `as` is for widening and lossless casts only (plus the two documented exceptions in the cast table: saturating float-to-int and fieldless-enum discriminants). [Design](references/design-and-errors.md#integer-and-float-casts)
6. **A panic is a proof.** `expect("why this cannot fail")` for a proven invariant; everything expected (input, I/O, network, state) returns `Result`. [Design](references/design-and-errors.md#panics-and-proofs)
7. **Every bound is declared.** Queues, buffers, retries, output and concurrency have a limit and a full/overflow policy. [Concurrency](references/verification-and-concurrency.md#own-concurrent-work)
8. **Async never blocks and never holds a `std` guard across `.await`.** Blocking work goes to `spawn_blocking`; `select!` branches are cancel-safe or own their partial progress; every spawned task has an owner. [Concurrency](references/verification-and-concurrency.md#async-rules)
9. **`unsafe` is isolated and proven.** Safe wrapper, `// SAFETY:` stating each obligation, privacy around the invariant, Miri where applicable. [Unsafe](references/verification-and-concurrency.md#unsafe-code-requires-a-proof)
10. **Follow the API Guidelines conventions.** Names, derives, conversions, visibility and `#[must_use]` as in [API conventions](references/api-conventions.md); Clippy's groups encode the rest.

## Read the relevant reference

Load only the reference the current decision needs:

| Decision | Reference |
| --- | --- |
| Names, derives, conversions, visibility, module layout, attributes, comments | [API conventions](references/api-conventions.md) |
| Types, enums, traits, state models, control flow, errors, casts, panics | [Design and errors](references/design-and-errors.md) |
| Borrowing, cloning, `Copy`, `Cow`, iteration, allocation, smart pointers | [Ownership](references/ownership.md) |
| Tests, test support, Clippy, performance claims, async, channels, unsafe | [Verification and concurrency](references/verification-and-concurrency.md) |
| Reviewing a diff | [Review checklist](references/review-checklist.md) |

Check version-sensitive claims against the project's toolchain and the primary documentation linked at the end of each reference. External examples inform a decision; they do not authorize new dependencies, upgrades or wider work.

## Finish with evidence

Run the project's checks for what you touched. A behavior-preserving refactor relies on existing tests; an intentional change is verified at its existing test boundary. Report what changed, the meaningful tradeoff, the checks actually run and their limits. Record a newly discovered project invariant where the project keeps its rules; keep reusable Rust advice here.
