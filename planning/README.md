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

When implementation completes, accepted behavior moves into the owning spec and the execution plan is deleted in the same change (also when the work is still local). Unresolved defects and deferred checks go to `NEXT_STEPS.md`; update inbound links; Git/PR/CI history keeps completed results. Retain a plan only when the user explicitly asks for a historical record; a scoped security finding may retain the evidence needed for its reasoning.

## Active plans

| Plan | Remaining scope |
|---|---|
| [Tool protocol and failure visibility](./tools-protocol/README.md) | Seven slices: honest failure causes, fixable refusals, failed turns in history, optional plan Tool, text answers with `tool_choice: auto`, slash Skill invocation, peer-style Tool rows |
| [Engine loop: decide, then apply](./engine-loop/README.md) | Nine slices after tools-protocol slice 5: closed stop causes, one primary settlement, child and worker decisions, the worker cause relabel, the child-loop split, the dispatch decision, a closed `RunStop` at the edges, the idiom sweep and bounded journaled Provider retries |
| [Terminal parity: scroll, copy and inline dialogs](./terminal-parity/README.md) | Eight slices: Classic by default with native scroll and transcript dump, content-sized Classic viewport, inline numbered approval, inline selectors, gesture-only clipboard writer and `/copy`, Fullscreen capture with selection and copy, Fullscreen pager, Fullscreen by default |
| [Agentic coding teams](./agentic-teams/README.md) | C2 edit-only workers, C3 command-capable views, roster presets, RPI, Supervisor adapters and deferred Goal/fetch/Git workflows |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
