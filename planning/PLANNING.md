# Planning workflow

Rules for maintained execution plans. [README.md](./README.md) defines the folder mechanism and [TEMPLATE.md](./TEMPLATE.md) defines the shared structure.

## Rules

- Put repository-level plans under `planning/`, not in ad hoc documentation folders.
- Edit an existing plan only when the user asks to change or continue it. Analysis does not silently mutate the plan.
- Resolve checkable facts before finalizing. Turn unresolved material uncertainty into a concrete open question.
- Sweep for placeholder language such as `assume`, `probably`, `maybe`, `TBD`, `later`, and `follow up`; resolve it or make the decision hole explicit.
- Split work by responsibility and risk, not arbitrary file groups.
- Each branch or PR should be describable in one sentence and leave the repository coherent.
- Change status to `In progress` when implementation starts.
- For multi-PR plans, update the change log with shipped work before starting the next slice.
- Remove the plan when its final slice lands unless the user requests a historical record.
- Cite adjacent rules and research instead of restating them.
- For behavior work, link the owning spec or unmigrated contract and its existing evidence owner. Describe proposed changes with successful and meaningful rejection/failure/recovery scenarios before implementation. Review contract impact after each planning pass using [the spec workflow](../docs/specs/README.md), which owns acceptance, updates, extraction and evidence status.

## Harness-specific questions

Every architecture or module plan resolves:

- Which module owns each invariant and piece of state?
- What is the module's interface, including ordering, errors, cancellation, and performance characteristics?
- Is each proposed seam in-process, local-substitutable, remote-owned, or truly external?
- Which concrete adapters justify the seam today?
- Which data crosses the seam, and which external types must be translated at the edge?
- How are compatibility, capability negotiation, and event replay handled across language or process boundaries?
- What is bounded: queues, buffers, bodies, artifacts, concurrency, retries, and tool output?
- What is the backpressure or overflow behavior for every bound?
- What is the security model for credentials, permissions, tools, files, network access, memory scope, and untrusted content?
- What latency, resource, correctness, and recovery evidence decides whether the design succeeds?
- Which tests exercise the module through its interface, including cross-language golden fixtures when a protocol changes?

## Process

1. Define the outcome and the user or system behavior it enables.
2. Map current facts and ownership before proposing crates or directories.
3. Separate policy from details and identify information each module must hide.
4. Design the interface and failure semantics before the implementation layout.
5. Compare at least two materially different designs for hard-to-reverse interfaces.
6. Select seams only where variation, isolation, or an external dependency earns them.
7. Order implementation into coherent, verifiable slices.
8. Define exit criteria and measurements before implementation begins.
9. Run a final assumption, security, compatibility, and auto-learning sweep.

## Smell test

- A crate exists only to mirror an architecture diagram → merge it until a dependency or ownership reason appears.
- An adapter only forwards calls and has no alternative → the seam is probably hypothetical.
- Transport or database types cross into policy → translate them at the edge.
- The same state can be mutated by several modules → assign one owner.
- A client needs internal engine knowledge → deepen the engine interface.
- A plan says “plugin” without a capability, lifecycle, trust, and versioning model → it is not designed yet.
- A plan contains a language choice without a workload or interoperability reason → state the reason or defer the choice.
- A section can disappear without losing decision-relevant information → delete it.
