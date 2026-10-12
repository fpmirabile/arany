---
name: clean-architecture-review
description: Review architecture through Robert C. Martin's Clean Architecture and SOLID principles. Use for dependency-direction audits, responsibility and contract reviews, or prioritizing architectural improvements; not ordinary style-only code review.
---

# Clean Architecture review

Use principles to explain concrete change and failure risks, not to score conformance to a diagram. Repository rules and the user's scope govern implementation. A review is read-only unless changes are explicitly requested.

## Procedure

1. Load the project's architecture and relevant module rules. For Arany, apply the [product and implementation priorities](../../../AGENTS.md#product-and-implementation-priorities), including Ponytail and OS parity. Map actual callers, source dependencies, state owners, adapters and privilege boundaries. Follow at least one complete successful operation and its failure/cancellation path before drawing conclusions.
2. Apply the lenses below to that map. Record strengths as well as problems. File length, a concrete dependency or the absence of a trait is not independently a finding.
3. For each finding, cite current file/line and caller chain, the violated contract or independent reason to change, concrete consequence, confidence, smallest repair and existing verification owner. Separate demonstrated defects, justified structural improvements and preference-only suggestions. Deduplicate symptoms with the same owner/root cause.
4. When parallel review is requested or authorized, divide it into dependency direction, responsibility/ownership, and behavioral contracts. Reviewers first work independently; reconcile overlapping or contradictory findings against the code before assigning disjoint implementation ownership. Otherwise review sequentially.
5. If implementation is authorized, apply confirmed defects and bounded high-value improvements. Preserve ordering, errors, cancellation, persistence compatibility, security and resource bounds. Ask before changes requiring additional authority, such as public API renames, module restructuring or shared infrastructure. Defer speculative generalization with a concrete trigger.
6. Verify through existing public-interface, replay and protocol owners; strengthen an existing scenario only for a newly demonstrated gap. Review the final diff and update the authoritative architecture/rules when ownership or an invariant changed. Report applied, deferred and rejected recommendations separately, with verification scope and remaining uncertainty.

Completion means every reported finding has a disposition and every applied change has evidence proportional to its risk. Passing tests alone does not prove architectural quality.

## Review lenses

- **Dependency rule / DIP:** distinguish source dependency from runtime control flow. Stable policy should consume domain-shaped inputs/results; UI, wire, database-row and platform types stay with their adapters. Check whether errors and configuration smuggle those details back inward. Invert a demonstrated dependency when needed, not every concrete call.
- **SRP:** identify the actor or independent reason for change. Group policy that changes together under one owner; separate independently changing responsibilities. Many functions can implement one cohesive responsibility. A file split without an ownership improvement is cosmetic.
- **OCP:** locate actual variation and ask whether adding an existing kind of adapter changes stable policy. Prefer a narrow established extension point; introducing hypothetical plug-ins or a trait per implementation does not earn openness.
- **LSP:** compare substitutable implementations' complete observable contracts: accepted inputs, outcomes, errors, cancellation, deadlines, disclosure, cost and persistence semantics. Legitimate declared capability differences are not defects; silently relaxing the common contract is.
- **ISP:** inspect each caller's required operations and knowledge. Prefer small domain-shaped interfaces, avoiding callers that must understand unrelated internals or supply meaningless fields. One cohesive interface need not become several traits.
- **Component cohesion and coupling:** trace cycles, duplicated policy, shared mutable ownership and change blast radius. Treat Rust modules as boundaries even within one package. Preserve justified process/privilege boundaries. Create crates only for measured dependency, ownership or release pressure.

For Arany, use the project's deep-module vocabulary alongside these lenses; it is a complementary design approach, not attributed to Martin. Concrete SQLite ownership and separate credential/Guard processes have real lifecycle and privilege reasons. Provider already has multiple adapters. Preserve these reasons unless evidence supports a better design.

## Attribution

Paraphrase the principles; do not reproduce book passages. Primary references:

- [The Clean Architecture](https://blog.cleancoder.com/uncle-bob/2012/08/13/the-clean-architecture.html): inward source dependencies and boundary data.
- [The Single Responsibility Principle](https://blog.cleancoder.com/uncle-bob/2014/05/08/SingleReponsibilityPrinciple.html): responsibilities follow reasons and people driving change.
- [The Open Closed Principle](https://blog.cleancoder.com/uncle-bob/2014/05/12/TheOpenClosedPrinciple.html): protect stable behavior while extending real variation.
- [SOLID Relevance](https://blog.cleancoder.com/uncle-bob/2020/10/18/Solid-Relevance.html): principles apply beyond inheritance-heavy object-oriented designs.
