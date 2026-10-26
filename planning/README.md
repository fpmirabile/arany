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
| [Architecture residue](./architecture-residue/README.md) | `In progress`; R1 diagnostic cycle, R2 bounded setup/error flow moves, R3 Drafts/focused Run tests, R4 Transcript, R5 native Provider-name residue; R2c5/R6b closes only after the evidence checklist is complete |
| [Image chip hover preview](./image-hover-preview/README.md) | `In progress`; S1 Kitty and S2 iTerm2 implemented; only S3 Sixel remains, gated on `png` dependency approval and serialized with architecture-residue R4 |
| [Spec limits generation](./spec-limits-generation/README.md) | `In progress`; S1/S2 exports implemented; S3 blocking spec-budget/restatement check after architecture-residue R1–R5 spec edits, then S4 native Tools split |
| [Context budgets](./context-budgets/README.md) | `In progress`; remaining output spill S3a–S3d and separate storage report; compaction summary budget and recent-file hints implemented |
| [OpenAI Chat Completions family](./openai-chat-completions-family/README.md) | `In progress`; O7 (2026-10-26): remaining S2 protocol inspection label and setup-journey assertion proceed now in `cli/attached/setup.rs`; architecture-residue R2b carries them through its moves |
| [Commands on a Workspace overlay](./command-workspace-overlay/README.md) | `Ready`; O1 (2026-10-26), B+C+E: S1a1 supervisor/tmpfs/probe on copies (560 lines), S1a2 whole-root overlay/masks/launch baseline/regular-file deletion (600), S1b per-root grants (490), S1c opaque/subtree/recreation (580); supervisor mounts overlays and binds merged paths into bwrap; S2a/S2b cache after S1c and #446, then S3 CPU/jobs (380); Linux disclosure/floor non-claims explicit, macOS keeps copies |
| [Guard egress proxy, web Tools and remote MCP](./network-proxy/README.md) | `Ready`, low priority; after the overlay plan: S1 allow-listed egress proxy for guarded commands, S2 `web_fetch`/`web_search`, S3 remote MCP with OAuth |
| [Editors on the shared Workspace](./shared-workspace-editors/README.md) | `Ready`; O2 (2026-10-26): S1 depends only on landed in-process-file-mutations S2 and G5; live-tree editors use symmetric leases, checkpoints and verification; S2a/S2b retire the private handover protocol; S1 commands retain snapshots and use exclusive cache-identity leases; S3 deletes native private targets after overlay S2b. Non-goal: per-Goal worktree stays opt-in on its recorded trigger |
| [Assistant Markdown rendering](./markdown-rendering/README.md) | `Ready`, normal/medium priority after the plans above; S1 committed prose, S2 code/lists/quotes, S3 tables, S4 streaming/incomplete answers; `pulldown-cmark =0.10.3` approved; implementation follows architecture-residue R4 and serializes shared terminal work |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
