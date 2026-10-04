# Clean Architecture principles for Arany

**Date:** 2026-10-04  
**Scope:** primary-source interpretation for an architecture review, not a new product contract.

## What Martin's sources establish

Martin separates policy from mechanisms through inward source dependencies. Runtime control flow may go outward; an inward-facing interface and policy-shaped data prevent the mechanism's types from following it back into policy. His circles are schematic rather than a required number of layers. [The Clean Architecture](https://blog.cleancoder.com/uncle-bob/2012/08/13/the-clean-architecture.html).

The single-responsibility principle concerns independently changing responsibilities and the people specifying them, not one function per file. Martin connects it to information hiding and separation of concerns. [The Single Responsibility Principle](https://blog.cleancoder.com/uncle-bob/2014/05/08/SingleReponsibilityPrinciple.html).

The open-closed principle protects existing behavior while adding variation, by directing extension dependencies toward stable policy. Martin's plugin examples demonstrate that possibility; they do not establish that every application needs runtime plugins. [The Open Closed Principle](https://blog.cleancoder.com/uncle-bob/2014/05/12/TheOpenClosedPrinciple.html).

Martin's later summary applies SOLID beyond inheritance: substitutions must preserve the interface's meaning; callers should not depend on unrelated interface details; stable abstractions should remain independent of low-level mechanisms. [SOLID Relevance](https://blog.cleancoder.com/uncle-bob/2020/10/18/Solid-Relevance.html).

These are paraphrases. OCP and LSP have origins beyond Martin; this note uses his architectural interpretation without claiming he invented every principle.

## Application to this Rust harness

The following are review judgments for Arany, not claims from the articles:

- The semantic `Provider` contract is an earned substitution seam: native adapters and the scripted test adapter vary while Engine consumes the same request, outcome and failure categories.
- A concrete Store is not independently a defect. The relevant question is whether SQLite rows, SQL, connection lifetimes or driver errors steer policy outside the Store's interface. Its one-thread connection ownership serves real durability and blocking-I/O requirements.
- Guard and credential helpers have real privilege and lifecycle reasons. Do not replace those process boundaries with fakeable traits just to make a layer diagram uniform.
- Session, Provider and Tool semantic values currently reference one another within one package. Distinguish those domain references from dependencies on HTTP, terminal, SQLite or sandbox implementation types. A separate domain crate is warranted only by demonstrated ownership or dependency pressure.
- OCP does not authorize arbitrary Tool plugins, destinations, credentials or effect grants. New variation still requires the existing deterministic Policy, privilege enforcement, resource bounds and protocol evidence.
- An architectural finding needs a concrete caller chain, independent reason to change or failed contract, smallest repair and existing verification owner. Large files and absent traits are not enough.

The authoritative application contract remains [the system overview](../architecture/system-overview.md), and the repeatable procedure lives in [the review skill](../../agents/skills/clean-architecture-review/SKILL.md).
