# Planning workflow

Rules for maintained execution plans. [README.md](./README.md) defines the folder mechanism and [TEMPLATE.md](./TEMPLATE.md) the shared structure.

## Rules

- Put repository-level plans under `planning/`, not in ad hoc documentation folders. Edit an existing plan only when the user asks to change or continue it.
- Resolve checkable facts before finalizing; turn remaining material uncertainty into a concrete open question. Sweep for `assume`, `probably`, `maybe`, `TBD`, `later` and `follow up`.
- Split work by responsibility and risk. Each branch or PR is describable in one sentence and leaves the repository coherent.
- Set status to `In progress` when implementation starts; for multi-PR plans keep only remaining work and the next action, and close completed plans under the [planning lifecycle](./README.md#lifecycle), including local uncommitted work.
- Cite adjacent rules, specs and research instead of restating them. Match detail to the work: empty headings and repeated repository rules do not make a plan implementation-ready.
- For behavior work, link the owning spec and its evidence owner and describe the successful path plus meaningful rejection, failure and recovery scenarios before implementation; the [spec workflow](../docs/specs/README.md) owns contract updates.

## Harness-specific questions

Apply the [product and implementation priorities](../AGENTS.md#product-and-implementation-priorities): name the harness responsibility served, the simplest sufficient solution after Ponytail, and the OS impact (common behavior, necessary native differences, unavailable capabilities, native verification) or that there is none.

A plan that changes a module boundary or interface also answers, for the affected seam only: which module owns each invariant and piece of state; the interface's ordering, errors, cancellation and performance; whether the seam is in-process, local-substitutable, remote-owned or external and which concrete adapters justify it today; which data crosses it and which external types are translated at the edge; what is bounded (queues, buffers, bodies, artifacts, concurrency, retries, tool output) and its overflow behavior; the security model for credentials, permissions, tools, files, network, memory and untrusted content; and which tests exercise the module through its interface.

## Smell test

- A crate exists only to mirror an architecture diagram → merge it until a dependency or ownership reason appears.
- An adapter only forwards calls and has no alternative → the seam is probably hypothetical.
- Transport or database types cross into policy → translate them at the edge.
- The same state can be mutated by several modules → assign one owner.
- A client needs internal engine knowledge → deepen the engine interface.
- A plan says "plugin" without a capability, lifecycle, trust and versioning model → it is not designed yet.
- A plan contains a language choice without a workload or interoperability reason → state the reason or defer the choice.
- A section can disappear without losing decision-relevant information → delete it.
