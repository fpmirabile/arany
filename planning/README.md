# Planning

Use `planning/` for implementation plans, rollout plans, investigations, migrations, and other maintained execution work. Investigation findings belong in `docs/research/`; accepted behavior and decisions belong in the [owning spec](../docs/specs/README.md), or the [system overview](../docs/architecture/system-overview.md) for architecture shape. Link existing rationale instead of creating a parallel decision record.

Read [PLANNING.md](./PLANNING.md) before writing a plan and start from [TEMPLATE.md](./TEMPLATE.md).

Scale the artifact to the task. A bounded change can use a concrete plan in the
conversation; create a repository plan when work needs durable coordination,
multiple slices or a later handoff. Keep only substantive template sections.
Neither a template nor an unresolved proposal expands the user's authorized scope.

## Mechanism

- Create one kebab-case folder per initiative.
- Put its main plan in `README.md`.
- Keep the plan decision-oriented: one feature, its research, constraints, chosen direction, risks, vertical slices, and exit criteria ([PLANNING.md](./PLANNING.md)).
- Update the same plan as decisions change.
- Resolve facts from code, configuration, research, and authoritative documentation. Promote the remaining material uncertainty to `Open questions`.
- A plan is implementation-ready only when every material assumption is resolved or explicit.
- Each vertical slice is one PR; a feature small enough for one reviewable PR is one slice.
- When work starts or lands, update its status and next action.

## Conventions

- Repository content is English.
- Folder names are descriptive kebab-case without dates or numeric prefixes.
- Diagrams, fixtures, benchmarks, and initiative-specific notes live beside the plan.
- A supporting document must answer a distinct question; avoid distributing one decision across several files.

## Status

- `Draft`
- `Ready` (Research and Plan gates passed; no material open questions)
- `Ready`
- `Ready` — Research and Plan gates passed; no material open question remains.
- `Ready` (Research and Plan gates passed; implementation may await a named dependency)
- `Ready` (Plan gate passed; implementation may depend on a linked slice)
- `In progress`
- `Blocked`
- `Done`

`Draft` may contain open questions. `Ready` means Research and Plan gates passed, with no material open question; implementation has not started.

## Lifecycle

When implementation completes, accepted behavior moves into the owning spec and the execution plan is deleted in the same change (also when the work is still local). The closing PR carries the [slice delivery](./PLANNING.md#slice-delivery) checklist; unmet criteria and deferred checks go to `NEXT_STEPS.md` or a direct PR first. Update inbound links; Git/PR/CI history keeps completed results. Retain a plan only when the user explicitly asks for a historical record; a scoped security finding may retain the evidence needed for its reasoning.

## Active plans

| Plan | Remaining scope |
|---|---|
| [Architecture residue](./architecture-residue/README.md) | `In progress`; R1 diagnostic cycle, R2 bounded setup/error flow moves, R3p1/E8b → R3p2/E8c Engine permission/cancellation tests in parallel with R3a (#484) → R3e Drafts/focused Run tests, R4 Transcript (#485, open), R5 native Provider-name residue; R2c5/R6b closes only after the evidence checklist is complete |
| [Image chip hover preview](./image-hover-preview/README.md) | `In progress`; S1 Kitty and S2 iTerm2 implemented; only S3 Sixel remains, gated on `png` dependency approval and serialized with architecture-residue R4 |
| [Spec limits generation](./spec-limits-generation/README.md) | `In progress`; S1/S2 exports implemented; S3 blocking spec-budget/restatement check after architecture-residue R1–R5 spec edits, then S4 native Tools split |
| [Context budgets](./context-budgets/README.md) | `In progress`; remaining output spill S3a–S3d and separate storage report; compaction summary budget and recent-file hints implemented |
| [OpenAI Chat Completions family](./openai-chat-completions-family/README.md) | `In progress`; S1 and S2 recipe/form contract landed; protocol inspection label and setup journey assertion remain after architecture-residue R2p2 and R2b1–R2b4 |
| [Commands on a Workspace overlay](./command-workspace-overlay/README.md) | `Ready`; S1 Linux bubblewrap overlay of the Workspace with upper-layer changesets, S2 build cache as a bind mount (after #446), S3 all-cores Build CPU and unpinned jobs with `cargo test` measured inside the Guard; macOS keeps copy semantics, documented |
| [Guard egress proxy, web Tools and remote MCP](./network-proxy/README.md) | `Ready`, low priority; after the overlay plan: S1 allow-listed egress proxy for guarded commands, S2 `web_fetch`/`web_search`, S3 remote MCP with OAuth |
| [Test corpus pruning](./test-corpus-pruning/README.md) | `Ready`; T1a–T1c reducer, T2a–T2b Goal scenarios, T3a–T3b worker/Tools after shared-workspace-editors S2, T4a–T4c permissions/setup/view, T5a–T5e Engine/CLI protocol journeys, T6a Event families; journey roots and migration evidence excluded |
| [Display facts](./display-facts/README.md) | `Ready`; after shared-workspace-editors S2 and before network-proxy S2: D1 library `DisplayFacts` consumed by Tool rows and serve items, D2 ACP and the journaled report consume it |
| [Editors on the shared Workspace](./shared-workspace-editors/README.md) | `Ready`; after overlay S1: S1 editors edit the live tree under symmetric leases with checkpoints and verification; S2 retires the base/overlay/WorkerData handover protocol in 2a/2b PRs with raised floors and an archive fixture; S3 delete native private worker targets. Non-goal: per-Goal worktree stays opt-in on its recorded trigger |
| [Assistant Markdown rendering](./markdown-rendering/README.md) | `Ready`, normal/medium priority after the plans above; S1 committed prose, S2 code/lists/quotes, S3 tables, S4 streaming/incomplete answers; `pulldown-cmark =0.10.3` approved; implementation follows architecture-residue R4 and serializes shared terminal work |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
