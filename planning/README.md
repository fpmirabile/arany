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
| [Visible reliability](./visible-reliability/README.md) | `Draft`; O1/O2 owner decisions remain; R1 native-store diagnostics/stability and R2 permission-family recurrence gate only VR3/VR4. VR1–VR4 independent: VR1 latency corpus (820), VR2 evidence-first topology closure (≤120; conditional repair 310/600), VR3 closed-category Locked diagnostics/stability (≤200/400), VR4 full-family recurrence (≤150 evidence-only; 880/1,400 provisional until R2). VR5 observer disposal/closeout (1,000) lands last; VR1/VR5 implementation scope is ready; estimates count additions plus deletions. |
| [Reported Provider limits](./usage-limits/README.md) | `Draft`; UL1 route diagnostics and honest `/usage` clause (250) → UL2a OpenAI JSON/report (900) → UL2b JSON compaction/paging (300) → UL3 streaming/attempts (850) → UL4 Anthropic (1,250 total) → UL5 custom routes/closeout (1,350 total); explicit UL4/UL5 flow splits above 1,200 measured lines, hard ceiling 1,500 per ID; D1 UL1-only freeze lift, D2 public route/private-endpoint prohibition, D3 diagnostic-first disposition accepted |
| [Guard egress proxy, web Tools and remote MCP](./network-proxy/README.md) | `Ready`, low priority; after the overlay plan: S1 allow-listed egress proxy for guarded commands, S2 `web_fetch`/`web_search`, S3 remote MCP with OAuth |
| [Test corpus pruning](./test-corpus-pruning/README.md) | `Ready`; T1a–T1c reducer, T2a–T2b Goal scenarios, T3a–T3b worker/Tools after shared-workspace-editors S2b2 (optional early T3a as shared-editor S0b), T4a–T4c permissions/setup/view, T5a–T5e Engine/CLI protocol journeys, T6a Event families; journey roots and migration evidence excluded |
| [Editors on the shared Workspace](./shared-workspace-editors/README.md) | `Ready`; S0a native/rewind pure move, optional S0b/T3a, S1 atomic live-editor cutover with same-PR migration and dated D1 exception; combined S1b1/S1b2 enables background reports and primary-path commands; combined S2b1/S2b2 retires transport/handover vocabulary and opens dependent plans; combined S2b3/S3 removes targets/native storage after overlay S2b. O2: no overlay prerequisite for early slices. D2: S2c' verifies child commands after overlay S3, removable at closing if existing evidence suffices. Non-goal: per-Goal worktree stays opt-in on its recorded trigger |
| [Remaining Markdown syntax](./markdown-syntax/README.md) | `Ready`; D1–D3 resolved (row-break soft breaks, 1,500 ceiling, extra syntax literal); S1b paragraphs/inline styles (1,050) → S1c direct inert links (550) → S1d decoded/reference links and projection coordinate (1,300) → S2b decoded fence labels (380) → S2c lists/quotes/nested code and closeout (1,000); no dependency or OS impact; syntax highlighting excluded |
| [Terminal dogfood fixes](./terminal-dogfood-fixes/README.md) | `Draft`; A1 approval labels, session offer and Auto edits hint → A2 leading colored sensitive-path warning → C1 closing checklist |
| [Git writes on macOS](./git-write-macos/README.md) | `Draft`; M0 Linux plan/publish split (1,100) and M0b bounded Git view volume (500) → M1a macOS branch creation via executor and publisher units (1,350) → M1b commit (700) → M2 switch (800) → M3 closing (300); four owner decisions open |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
