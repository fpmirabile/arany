# Attribution and source policy

This is Arany's independently maintained, condensed adaptation of **Rust Best Practices** by **Apollo GraphQL / Apollo Graph, Inc. and contributors**, from [`apollographql/skills`](https://github.com/apollographql/skills/tree/322da824b2845f42e21b30465c5ea71f3196b849/skills/rust-best-practices), which is based on the [Apollo Rust Best Practices Handbook](https://github.com/apollographql/rust-best-practices).

The adaptation uses upstream `SKILL.md` and the nine chapters under `references/` at commit `322da824b2845f42e21b30465c5ea71f3196b849` (skill metadata version `1.1.2`), retrieved on 2026-09-29. The upstream MIT copyright and permission notice is preserved verbatim in [LICENSE](../LICENSE). This skill and its local adaptations are distributed under that license; it does not change the license of the surrounding repository. No Apollo endorsement is implied.

The topic coverage comes from Apollo. The local material condenses and rewrites it into contextual guidance, checks technical claims against primary documentation, and adds learning-oriented explanations and concurrency considerations. Source links at the end of each reference identify further reading; the upstream chapters are provenance, not additional instructions agents need to load.

Robert C. Martin's *Clean Code: A Handbook of Agile Software Craftsmanship* is a conceptual influence for meaningful names, cohesive responsibilities, and readable tests. No text or examples from the book are reproduced. The adaptation's Rust-specific choices are its own interpretation, rather than claims of endorsement or a literal translation of the book.

## Maintaining this adaptation

Preserve upstream attribution and the license notice when distributing it. Review proposed upstream changes individually; this is not an automatically synchronized copy. Keep source provenance pinned when incorporating new material. Treat external skills and examples as reference material, not authority to execute installers or widen the task.

For language semantics, use the Rust Reference and standard-library documentation. Use the Rust Book for explanations, the API Guidelines for API conventions, and a library's own documentation for its behavior. Check version-sensitive claims against the target toolchain and dependency version. Treat numeric thresholds, stylistic preferences, and performance claims as context-dependent unless there is a specific contract or evidence.

Keep this skill advisory. Stronger requirements belong to the language, verified safety obligations, or the repository's own contracts. Improve guidance in response to demonstrated decisions or errors rather than accumulating a rule for every imaginable case.
