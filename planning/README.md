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
- Status changes when work starts and when the plan closes; slice progress is recorded under [slice delivery](./PLANNING.md#slice-delivery).

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
| [Architecture residue](./architecture-residue/README.md) | `In progress`; R1 diagnostic cycle, R2 remaining bounded setup moves from R2p5, R3p1/E8b implemented pending CI evidence → R3p2/E8c Engine cancellation/clock tests (R3a–R3e Run ownership implemented; execution evidence pending), R4 Transcript (#485, open), R5 native Provider-name residue; R2c5/R6b closes only after the evidence checklist is complete |
| [Image chip hover preview](./image-hover-preview/README.md) | `In progress`; S1 Kitty and S2 iTerm2 implemented; only S3 Sixel remains, gated on `png` dependency approval and serialized with architecture-residue R4 |
| [Spec limits generation](./spec-limits-generation/README.md) | `In progress`; S1/S2 exports implemented; S3 blocking spec-budget/restatement check after architecture-residue R1–R5 spec edits, then S4 native Tools split |
| [Context budgets](./context-budgets/README.md) | `In progress`; remaining separate output-spill storage report; explicit paged Read, retained-output journeys, compaction summary budget and recent-file hints implemented |
| [Commands on a Workspace overlay](./command-workspace-overlay/README.md) | `Draft`; both stock-Ubuntu entry routes refused in S1a0; D7 owner resolution independently gates S1a1; G1'a/G1'b1/G1'b2 → S1a1 supervisor/copy path (590) → S1a2a view/masks/D5/attestation (590) → S1a2b baseline/create-edit/conflicts (590) → S1b narrow grants (520) → S1c1 deletions/dedup (510) → S1c2 subtree/recreation (520) → S1d primary copy-transport retirement (590) → S2a default cache overlay (590) → S2b Build cache overlay/archive retirement (590) → S3 CPU quota/settings (320); D6 keeps cache uppers bounded, Cargo jobs stay G1'b1, macOS keeps copies; [slice delivery](./PLANNING.md#slice-delivery) |
| [Guard toolchain defaults for daily development](./guard-toolchains/README.md) | `In progress`, daily-use priority, both OSes; oversized G1'b replaced by G1'b1 journaled Build jobs (480) → G1'b2 validated timeout cache/macOS repository acceptance and closure (590); optional G1'c triggered private range (260), optional G2' root literals after overlay S2b (120); G1'a/G1'b1/G1'b2 precede overlay S1a1; Cargo jobs belong to G1'b1 |
| [Guard egress proxy, web Tools and remote MCP](./network-proxy/README.md) | `Ready`, low priority; after the overlay plan: S1 allow-listed egress proxy for guarded commands, S2 `web_fetch`/`web_search`, S3 remote MCP with OAuth |
| [Test corpus pruning](./test-corpus-pruning/README.md) | `Ready`; T1a–T1c reducer, T2a–T2b Goal scenarios, T3a–T3b worker/Tools after shared-workspace-editors S2b2 (optional early T3a as shared-editor S0b), T4a–T4c permissions/setup/view, T5a–T5e Engine/CLI protocol journeys, T6a Event families; journey roots and migration evidence excluded |
| [Display facts](./display-facts/README.md) | `Ready`; after shared-workspace-editors S2b2 and before network-proxy S2: D1 library `DisplayFacts` consumed by Tool rows and serve items, D2 ACP and the journaled report consume it |
| [Editors on the shared Workspace](./shared-workspace-editors/README.md) | `Ready`; S0a native/rewind pure move, optional S0b/T3a, S1 atomic live-editor cutover with same-PR migration and dated D1 exception; combined S1b1/S1b2 enables background reports and primary-path commands; combined S2b1/S2b2 retires transport/handover vocabulary and opens dependent plans; combined S2b3/S3 removes targets/native storage after overlay S2b. O2: no overlay prerequisite for early slices. D2: S2c' verifies child commands after overlay S3, removable at closing if existing evidence suffices. Non-goal: per-Goal worktree stays opt-in on its recorded trigger |
| [Assistant Markdown rendering](./markdown-rendering/README.md) | `Ready`, normal/medium priority after the plans above; S1 committed prose, S2 code/lists/quotes, S3 tables, S4 streaming/incomplete answers; `pulldown-cmark =0.10.3` approved; implementation follows architecture-residue R4 and serializes shared terminal work |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
