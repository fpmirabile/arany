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
- `In progress`
- `Blocked`
- `Done`

`Draft` may contain open questions. Material open questions prevent implementation-ready status.

## Lifecycle

When implementation completes, accepted behavior moves into the owning spec and the execution plan is deleted in the same change (also when the work is still local). Unresolved defects and deferred checks go to `NEXT_STEPS.md`; update inbound links; Git/PR/CI history keeps completed results. Retain a plan only when the user explicitly asks for a historical record; a scoped security finding may retain the evidence needed for its reasoning.

## Active plans

| Plan | Remaining scope |
|---|---|
| [Follow AGENTS.md instructions like Codex](./agents-md-instructions/README.md) | Two slices after root framing: nested root-to-launch-directory instructions with a shared cap, user-global `STATE_DIR/AGENTS.md` |
| [Git write workflow](./git-write-workflow/README.md) | Three slices: guarded local Git writes, user-owned typed prefix rules, human-approved host push and PR creation |
| [Switch between agent chats](./agent-chat-switching/README.md) | Four slices: read-only child transcript switching, committed live updates, searchable `/agents` switch picker, messaging the selected child |
| [Automatic state migrations](./state-migrations/README.md) | Four slices: automatic archive at startup, journal migration framework and guard, checkpoint store migration, JSON record migration |
| [Agentic coding teams](./agentic-teams/README.md) | C2 edit-only workers, C3 command-capable views, roster presets, RPI, Supervisor adapters and deferred Goal/fetch/Git workflows |
| [Search with regex, file filtering and paging](./search-regex/README.md) | One slice: regex/include/case controls and deterministic pages of at most 100 rows |
| [Read-only Glob Tool](./glob-tool/README.md) | One slice: bounded in-process file discovery with regex-backed glob matching and deterministic path order |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
