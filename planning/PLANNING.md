# Planning workflow

Rules for maintained execution plans. [README.md](./README.md) defines the folder mechanism and [TEMPLATE.md](./TEMPLATE.md) the shared structure.

## Rules

- Put repository-level plans under `planning/`, not in ad hoc documentation folders. Edit an existing plan only when the user asks to change or continue it.
- One plan delivers one feature: a single capability a user, client or test can observe. Two or more features get two or more plans, each with its own lifecycle; link a dependency instead of merging them.
- Work in Research → Plan → Implement (RPI) order, and do not start the next phase until the previous one passes its gate:
  - **Research:** verify the current code, the owning spec and the peer harness behavior from source. Record the findings with citations in the plan's research section, or in `docs/research/` when they outlive the plan. Gate: no material claim rests on memory or assumption.
  - **Plan:** write the outcome, the scenarios and the vertical slices from those findings only. Gate: no material open question remains.
  - **Implement:** land slices in order. If implementation contradicts the research, stop and update the plan before continuing.
- Slice vertically. Each slice is a thin end-to-end increment, crossing every layer the feature touches (input or protocol, Engine, journal, Tools, presentation, spec and tests), that ships a behavior observable on its own and leaves the feature usable. Do not slice by layer (types first, all specs, then wiring) or by artifact. A refactor slices by flow: each slice moves one complete path and keeps observations byte-identical.
- Resolve checkable facts before finalizing; turn remaining material uncertainty into a concrete open question. Sweep for `assume`, `probably`, `maybe`, `TBD`, `later` and `follow up`.
- Each slice is one branch or PR under [slice delivery](#slice-delivery), describable in one sentence, and leaves the repository coherent. Split a slice further only by risk, never by layer.
- Set status to `In progress` in the PR that starts implementation, and close completed plans under the [planning lifecycle](./README.md#lifecycle), including local uncommitted work. Progress between those PRs follows [slice delivery](#slice-delivery).
- Cite adjacent rules, specs and research instead of restating them. Match detail to the work: empty headings and repeated repository rules do not make a plan implementation-ready.
- For behavior work, link the owning spec and its evidence owner and describe the successful path plus meaningful rejection, failure and recovery scenarios before implementation; the [spec workflow](../docs/specs/README.md) owns contract updates.

## Slice delivery

The unit of delivery, review and bookkeeping is the slice ID. A slice is at most 1,500 changed lines, excluding pure moves and listed deletions, unless its plan records an owner exception.

- A family of IDs is that many slices; narrowing a PR leaves the remaining IDs open. Only the branch under review delivers a slice.
- A PR names the exact IDs it completes in its `Slices` line and nothing more. An ID delivered in part keeps its ID and stays listed as remaining in the plan.
- A slice PR leaves the plan's slice list and its `README.md` index row unchanged: merged progress is the `Slices` lines of merged PRs, so parallel slices of one plan never conflict on bookkeeping. Only the PR that starts implementation, a PR that changes the plan's scope, slices or gates, and the closing PR edit the plan or its row.
- Reviewers check the claimed IDs against the diff and against the plan's slice text (files, inventory, ceiling, same-PR docs) before approving; an exit criterion wider than its inventory (a grep, a dependency-scan result) is checked by running it, not by reading the inventory.
- The PR that closes a plan (normally its last slice) carries a checklist: one line per slice ID naming its merged PR, and one line per exit criterion naming the command or evidence and its result. An unmet criterion moves to `NEXT_STEPS.md` or a direct PR before the plan is deleted; it is never deleted with the plan. The [lifecycle](./README.md#lifecycle) owns what happens to the plan file.

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
