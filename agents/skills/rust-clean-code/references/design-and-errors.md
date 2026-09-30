# Design and errors

Use Clean Code as a lens for intent and cohesion. The guidance below is this adaptation's interpretation, not a claim that the book prescribes Rust patterns.

## Names and boundaries

Choose names that reveal the domain operation, relevant units, and side effects. Keep vocabulary consistent with the project. A boolean is natural for a predicate; a named enum or separate operation may clarify a mode argument whose meaning is invisible at the call site.

Extract a function when its name explains a meaningful operation, isolates a tricky invariant, or centralizes a decision that must change consistently. A single caller can justify a helper. Similar-looking expressions can also remain separate when they represent independent decisions. Line counts, parameter counts, and a fixed number of duplicated call sites are prompts to inspect, not extraction thresholds.

Keep a cohesive flow together when splitting it makes the reader chase state through many helpers. A small interface that hides substantial implementation can be easier to maintain than many tiny public types. Place code together when it owns the same invariant or reason to change; use the repository's architecture to choose actual seams.

Local mutation is often the most direct expression of an algorithm. Keeping effects at meaningful boundaries can help reasoning and tests, but does not require a trait, wrapper, or separate layer around every operation.

## Concrete types, enums, and traits

Start with the shape the problem actually has:

- **Concrete type:** one implementation with no current need for substitution.
- **Enum:** a known set of alternatives; exhaustive matching helps keep cases visible.
- **Generic input parameter (`T: Trait` or argument-position `impl Trait`):** callers supply a concrete implementation while code shares a behavior contract. Generating code for each concrete type can help optimization, with possible compile-time and code-size costs. Return-position `impl Trait` instead hides a concrete type chosen by the implementation.
- **`dyn Trait`:** erase the concrete type to support runtime selection, mixed implementations, or a simpler interface that avoids spreading generics. Indirection may affect hot paths; its practical cost needs evidence. `&dyn Trait` borrows and needs no new heap allocation; `Box<dyn Trait>` owns.

A trait describes a meaningful capability or substitution boundary. An inheritance-style hierarchy, a trait for every struct, or generic parameters throughout a codebase do not follow automatically from separation of concerns. Both static and dynamic dispatch can support a clear design. Check dyn compatibility against the compiler and Rust Reference rather than a simplified list of forbidden method shapes.

## Model useful guarantees

Private fields with a validating constructor or a newtype can establish an invariant. An enum can replace combinations of flags that admit invalid states. Add this structure when it removes realistic misuse or repeated validation.

Typestate represents lifecycle stages as distinct types so only appropriate operations are available. It fits a protocol whose legal sequence is known in the calling code. Runtime state machines often fit an enum and validated transitions better, especially when loading persisted state or processing external events. A state marker cannot prove that an external resource remains available. Keep external failure handling even when call order is checked by types.

Prefer a representation carrying the data required by each state; a marker alongside contradictory optional fields still needs internal discipline. The complexity of generic state transitions should pay for a concrete guarantee, and typestate itself does not require unsafe code.

## Preserve useful errors

Use `Option` for meaningful absence and `Result` for failure the caller needs to handle. `Result<Option<T>, E>` is useful when both absence and failure are distinct outcomes. Propagate with `?` when propagation is the intended behavior; use a match when recovery or classification matters. Discard an error or substitute a default only when that behavior is part of the contract.

Choose typed errors where callers need stable distinctions or recovery. An application boundary concerned mainly with reporting may benefit from an erased error type with context. `thiserror` and `anyhow` are tools for those needs, not mandatory library/binary categories; use the project's existing approach and dependencies. Preserve underlying causes when useful, and expose domain distinctions without unnecessarily coupling a public API to implementation dependencies.

Expected malformed input, unavailable resources, and network failures usually deserve explicit handling. A proven internal invariant can justify `expect` if project policy permits it; its message should explain why failure should be impossible. Assess the panic's impact on the surrounding service or process. Substituting `unreachable!` for `unwrap` does not improve the proof, and a harmless-looking default can hide a real defect.

Add context where it contributes the failed operation or domain meaning, and report at the layer responsible for presentation. Avoid logging the same propagated error at every layer. Error strings and debug output need the same attention to sensitive data and untrusted content as any other output.

## Comments and public documentation

Prefer clear names and types for what the code does. Preserve concise comments explaining non-obvious invariants, ordering constraints, or tradeoffs. Rustdoc describes how callers use the API, including meaningful errors, panics, side effects, and safety obligations. Document why when that helps callers use the API correctly. Keep architecture rationale in the project's design documents; teaching a language concept does not by itself warrant a source comment.

Sources: [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/checklist.html), [trait objects](https://doc.rust-lang.org/book/ch18-02-trait-objects.html), [dyn compatibility](https://doc.rust-lang.org/reference/items/traits.html#dyn-compatibility), [panic or Result](https://doc.rust-lang.org/book/ch09-03-to-panic-or-not-to-panic.html), and [rustdoc examples](https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html).
