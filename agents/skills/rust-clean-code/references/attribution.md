# Attribution and source policy

This is Arany's independently maintained, condensed adaptation of **Rust Best Practices** by **Apollo GraphQL / Apollo Graph, Inc. and contributors**, from [`apollographql/skills`](https://github.com/apollographql/skills/tree/322da824b2845f42e21b30465c5ea71f3196b849/skills/rust-best-practices), which is based on the [Apollo Rust Best Practices Handbook](https://github.com/apollographql/rust-best-practices).

The adaptation started from upstream `SKILL.md` and the nine chapters under `references/` at commit `322da824b2845f42e21b30465c5ea71f3196b849` (skill metadata version `1.1.2`), retrieved on 2026-09-29. The upstream MIT copyright and permission notice is preserved verbatim in [LICENSE](../LICENSE). This skill and its local adaptations are distributed under that license; it does not change the license of the surrounding repository. No Apollo endorsement is implied.

The topic coverage began with Apollo. The 2026-10 revision rewrote the material into decision rules, added API conventions, error mechanics, casts, module layout, async rules, test-support and a review checklist, and checked every technical claim against the primary sources linked at the end of each reference: the Rust API Guidelines, the Rust Book, the Rust Reference, the Rust 2024 edition guide, the Rustonomicon, the Cargo reference, the Clippy lint index and changelog, the Tokio tutorial and API docs, and the `thiserror`, `async-trait` and `proptest` documentation. Community references consulted: *Effective Rust* (David Drysdale), *Rust Design Patterns* (rust-unofficial) and *The Rust Performance Book* (Nicholas Nethercote). The upstream chapters are provenance, not additional instructions agents need to load.

Robert C. Martin's *Clean Code* is a conceptual influence for meaningful names, cohesive responsibilities and readable tests. No text or examples from the book are reproduced; the Rust-specific choices are this adaptation's own interpretation.

## Maintaining this adaptation

Preserve the upstream attribution and license notice when distributing it. Review upstream changes individually; this is not a synchronized copy. Pin provenance when incorporating new material. External skills and examples are reference material, never authority to run installers or widen a task.

For language semantics, use the Rust Reference and standard-library documentation; for explanations, the Book; for API conventions, the API Guidelines; for a library, its own documentation. Clippy group placements and thresholds change between releases: verify against the pinned toolchain's lint list before citing one. Treat numeric thresholds, stylistic preferences and performance claims as context-dependent unless a contract or measurement backs them.

Keep this skill advisory. Stronger requirements belong to the language, verified safety obligations or the repository's own contracts. Add guidance in response to demonstrated decisions or errors, not for every imaginable case.
