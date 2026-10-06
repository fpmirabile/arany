# Planning

Use `planning/` for implementation plans, rollout plans, investigations, migrations, and other maintained execution work. Architecture decisions belong in `docs/adr/`; research belongs in `docs/research/`.

Read [PLANNING.md](./PLANNING.md) before writing a plan and start from [TEMPLATE.md](./TEMPLATE.md).

## Mechanism

- Create one kebab-case folder per initiative.
- Put its main plan in `README.md`.
- Keep the plan decision-oriented: goal, scope, current facts, constraints, chosen direction, risks, execution, and exit criteria.
- Update the same plan as decisions change.
- Resolve facts from code, configuration, research, and authoritative documentation. Promote the remaining material uncertainty to `Open questions`.
- A plan is implementation-ready only when every material assumption is resolved or explicit.
- Use one PR when the work is cohesive and reviewable. Split by concern only when one PR would become large, risky, or mixed.
- When work starts or lands, update its status and next action.

## Conventions

- Repository content is English.
- Folder names are descriptive kebab-case without dates or numeric prefixes.
- Diagrams, fixtures, benchmarks, and initiative-specific notes live beside the plan.
- A supporting document must answer a distinct question; avoid distributing one decision across several files.

## Status

- `Draft`
- `In progress`
- `Blocked`
- `Done`

`Draft` may contain open questions. Material open questions prevent implementation-ready status.

## Lifecycle

Completed implementation moves into the owning domain spec; delete its execution plan in the same change, including when the implementation is still local and uncommitted. Keep architecture, rationale and editing rules at their respective owners, dated evidence in existing verification records, and deferred user checks in `NEXT_STEPS.md`. Update inbound links and retain only plans with remaining execution work, unless the user explicitly requests a historical record. Git history preserves committed execution artifacts.
