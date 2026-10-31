# Planning

Use `planning/` for implementation plans, rollout plans, investigations, migrations, and other maintained execution work. Investigation findings that outlive a plan belong in the owning spec; accepted behavior and decisions belong in the [owning spec](../docs/specs/README.md), or the [system overview](../docs/architecture/system-overview.md) for architecture shape. Link existing rationale instead of creating a parallel decision record.

Read [PLANNING.md](./PLANNING.md) before writing a plan and start from [TEMPLATE.md](./TEMPLATE.md).

Scale the artifact to the task. A bounded change can use a concrete plan in the
conversation; create a repository plan when work needs durable coordination,
multiple slices or a later handoff. Keep only substantive template sections.
Neither a template nor an unresolved proposal expands the user's authorized scope.

## Conventions

- One descriptive kebab-case folder per initiative, without dates or numeric prefixes; its main plan is `README.md`, updated as decisions change. [PLANNING.md](./PLANNING.md) owns plan content, gates and [slice delivery](./PLANNING.md#slice-delivery).
- Diagrams, fixtures, benchmarks, and initiative-specific notes live beside the plan. A supporting document must answer a distinct question; avoid distributing one decision across several files.
- Status is one of `Draft`, `Ready`, `In progress`, `Blocked` or `Done`, optionally with a short parenthetical such as a named dependency. `Draft` may contain open questions. `Ready` means Research and Plan gates passed, with no material open question; implementation has not started.

## Lifecycle

When implementation completes, accepted behavior moves into the owning spec and the execution plan is deleted in the same change (also when the work is still local). The closing PR carries the [slice delivery](./PLANNING.md#slice-delivery) checklist; unmet criteria and deferred checks go to `NEXT_STEPS.md` or a direct PR first. Update inbound links; Git/PR/CI history keeps completed results. Retain a plan only when the user explicitly asks for a historical record; a scoped security finding may retain the evidence needed for its reasoning.

## Active plans

| Plan | Remaining scope |
|---|---|
| [Reported Provider limits](./usage-limits/README.md) | `In progress`; OWNER DECISIONS D2 (2026-10-27) and D4 (2026-10-28): Codex/Pi `openai-codex` backend and public Codex OAuth client profile replace the public ChatGPT route and SIWC profile. UL0a per-account keyring slots (900) → UL0b public-client sign-in/identity/migrations with route refusal (1,000) → UL0c refresh/revocation (450) → UL0d backend catalog (600) → UL1a backend request/SSE end to end (1,250) → UL1b plan windows/report (750); independent UL2a OpenAI JSON/report (900) → UL2b JSON compaction/paging (300) → UL3 streaming/attempts (850) → UL4 Anthropic (1,250 total) → UL5 custom routes/closeout (1,350 total). Complete-flow splits; hard ceiling 1,500 per ID. Scoped UL0a–UL1b freeze exception (UL0a–UL0d include the auth profile); owner live checks remain in NEXT_STEPS |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
