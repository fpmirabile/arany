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
| [Reported Provider limits](./usage-limits/README.md) | `Draft`; OWNER DECISION D2 (2026-10-27): Codex/Pi `openai-codex` backend replaces public ChatGPT inference route; auth compatibility gate remains open. UL1a backend/account/refresh/SSE end to end (1,450 conditional) → UL1b plan windows/report (750); independent UL2a OpenAI JSON/report (900) → UL2b JSON compaction/paging (300) → UL3 streaming/attempts (850) → UL4 Anthropic (1,250 total) → UL5 custom routes/closeout (1,350 total). Complete-flow splits; hard ceiling 1,500 per ID. Scoped UL1a/UL1b freeze exception; owner live checks remain in NEXT_STEPS |
| [Guard user environment](./guard-user-environment/README.md) | `Ready`; U1 contract text and pinned-shell `-c` refusal (260), U2a selection-boundary causes, typed Guard stage, development log (560) and U2b selection budget inside the command timeout (300) open now; U3a process-environment record (560) → U3b login-shell capture (480); U4 user programs and shims end to end on Linux with multi-select grants (1,180, after overlay G1′) → U5 the same on macOS (560) → U6a Command tools page, Rewrite hook role and probe (1,150) → U6b Prefix wrapper role and Claude Code settings import (700, closing checklist); [slice delivery](./PLANNING.md#slice-delivery) |
| [Guard egress proxy, web Tools and remote MCP](./network-proxy/README.md) | `Ready`, low priority; after the overlay plan: S1 allow-listed egress proxy for guarded commands, S2 `web_fetch`/`web_search`, S3 remote MCP with OAuth |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
