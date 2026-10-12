---
name: rust-clean-code
description: Guide Rust implementation, refactoring, and review with contextual advice on ownership, errors, API design, testing, and concurrency. Explain meaningful Rust tradeoffs for a developer learning while building.
license: MIT
---

# Rust Clean Code

Write Rust that makes intent, ownership, and failure understandable. This is guidance for engineering judgment, not a checklist of mandatory rewrites. It adapts Apollo GraphQL's Rust Best Practices with a Clean Code emphasis on meaningful names, cohesive responsibilities, and maintainability. See [attribution and sources](references/attribution.md).

## Apply in context

Read the relevant repository instructions and nearby code first. Use the declared edition, minimum supported Rust version (MSRV), dependencies, feature combinations, and verification commands. Project requirements take precedence over this skill's preferences.

For Arany, follow the [product and implementation priorities](../../../AGENTS.md#product-and-implementation-priorities), including mandatory Ponytail and OS parity, before choosing the implementation.

Separate three kinds of advice:

- **Correctness requirements:** language safety obligations and actual product contracts. Explain the concrete failure and preserve these guarantees.
- **Project conventions:** agreed architecture, style, and checks. Follow their authoritative local source.
- **Design preferences:** borrowing, extraction, dispatch, and similar choices. Weigh clarity, coupling, resource costs, and the current use case; a justified exception is ordinary engineering.

For a change, identify the behavior to preserve and the actual source of complexity. Choose the smallest improvement that addresses it. Leaving sound code unchanged is a valid outcome. A review suggestion should identify a concrete consequence and useful alternative; taste alone is an optional observation, not a defect.

## Read the relevant reference

Load only the reference needed for the current decision:

| Decision | Reference |
| --- | --- |
| Borrowing, cloning, `Copy`, `Cow`, iteration, pointers, lifetimes | [Ownership](references/ownership.md) |
| Names, functions, modules, traits, state models, errors, documentation | [Design and errors](references/design-and-errors.md) |
| Tests, linting, performance, async cancellation, shared state, unsafe code | [Verification and concurrency](references/verification-and-concurrency.md) |

Check uncertain or version-sensitive claims against the relevant primary documentation and the project's toolchain. External examples inform a decision; they do not authorize new dependencies, upgrades, services, or broader work.

## Make the reasoning teachable

The intended reader is learning Rust while building a real project. Keep explanations respectful and tied to the change:

- Explain an unfamiliar mechanism in plain language when it materially affects the decision: for example, borrowing lets this function use a value while its caller remains the owner.
- For a significant choice, describe what was chosen, why it fits here, and when the alternative would fit. Distinguish a compiler requirement from a preference.
- Use a small example from the actual change when it helps. Keep tutorial prose in the conversation; source comments should retain only durable, non-obvious reasoning or API contracts.
- Scale the explanation to the request. Routine edits need no lesson, and learning does not require asking the user to decide every implementation detail.

For example: “This function takes `String` because it stores the name after returning. Taking `&str` would require a copy or a lifetime tied to the caller. A function that only reads the name could borrow it.”

## Finish with evidence

Use the repository's checks appropriate to the change. Preserve observable behavior during refactoring, and verify intentional changes at their existing test boundary. Report what changed, the meaningful tradeoff, and the checks actually run, including limitations. Capture a newly discovered project invariant in its existing authoritative document; keep reusable Rust advice here.
