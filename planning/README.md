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

When implementation completes, accepted behavior moves into the owning spec and the execution plan is deleted in the same change (also when the work is still local). Unresolved defects and deferred checks go to `NEXT_STEPS.md`; update inbound links; Git/PR/CI history keeps completed results. Retain a plan only when the user explicitly asks for a historical record; a scoped security finding may retain the evidence needed for its reasoning.

## Active plans

| Plan | Remaining scope |
|---|---|
| [Architecture homogenization](./architecture-homogenization/README.md) | `Ready`: 62 implementation slices in 20 tracks plus one terminal lifecycle research slice (63 slices / 21 tracks); bounded refactors and eight follow-ups for four decided owners: cached admission, intent records, Engine Supervisor and migrated Provider identity |
| [Tool failure continuation](./failure-feedback/README.md) | `In progress`: C1 primary failure/decline results and sanitized continuation delivered; C2 delegated feedback remains; prior-Run cause reinjection is owned by the Session contract |
| [Provider setup UI](./provider-setup-ui/README.md) | `Ready`: shared durable checker available; three PRs for account/custom-route management, per-role routing and named roster editing in Fullscreen, Classic and linear modes |
| [Anthropic Tool turns](./anthropic-tool-stream/README.md) | `Ready`: S1 names Anthropic failure stages in `development.log`; S2 journals the closed reason; S3 fixes the cause after the owner's live re-run |
| [GitHub PR reads](./github-pr-reads/README.md) | Ready; depends on Git-write S3; S1 host PR listing with human or user-owned typed-prefix approval, S2 bounded PR discussion and checks |
| [Primary host commands](./primary-host-commands/README.md) | `Ready`: guarantee wording and H1 approved; S1 primary escalation, session approval and privilege-tool refusal, S2 user-owned host prefixes, S3 serve/ACP/exec |
| [Real-scale command snapshot](./real-scale-command-snapshot/README.md) | `In progress`: PR1 and PR2 are implemented; PR3 changeset capacity remains; `ignore` 0.4.33 is approved |
| [Gitignore-aware read discovery](./gitignore-read-discovery/README.md) | In progress: typed discovery/local suggestions await CI/native evidence; Git disclosure remains |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
