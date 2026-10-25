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
| [Architecture homogenization](./architecture-homogenization/README.md) | `In progress`: J1–J2/K1–K2 plus fifteen approved post-refactor families in ≤600-line code/test/spec/architecture PRs; P5 before K1, G5/C2 after J1, terminal/setup cuts after image-hover-preview S2/provider-setup-ui S3; C1–C3 clone cleanups; final slice deletes the plan |
| [Provider setup UI](./provider-setup-ui/README.md) | `In progress`: S1 account/custom-route management, then per-role routing and named roster editing in Fullscreen, Classic and linear modes |
| [Image chip hover preview](./image-hover-preview/README.md) | `Ready`; after image attachment chips (#425); S1 Kitty-protocol balloon with any-motion capture, S2 iTerm2 inline images, S3 Sixel gated on `png` dependency approval |
| [Spec limits generation](./spec-limits-generation/README.md) | `Ready`: S1 generated `limits.md` for Tool bounds, S2 Session/execution/Goal/Provider rows (after K1/K2), S3 spec word-budget and restatement check in CI, S4 `tools.md` native split (after C3) |
| [Multi-call Tool steps](./multi-call-steps/README.md) | `Ready`; after J1/J2, E8a–E8c, C2 (Engine) and P5/P4 (adapters): S1 1–4 calls per step with sequential dispatch, S2 Anthropic parallel calls, S3 concurrent in-process reads, S4 OpenAI/ChatGPT constant tool list and parallel calls |
| [Context budgets](./context-budgets/README.md) | `Ready`: S1 compaction summary budget, S2 recent-files list on the compaction record (after J2/K1), S3 output spill file and `spill:` Read (after J1/J2, E8a–E8c, C3; security pass) |
| [Tool-step Event folding](./tool-event-folding/README.md) | `Ready`; after J1/J2/K1/C2, E8a–E8c and B14: S1 verdict folded into `ToolStarted` with floor/archive fixture, S2 Engine pre-validates through the reducer (after G5), S3 lenient `show` projection |
| [OpenAI Chat Completions family](./openai-chat-completions-family/README.md) | `Ready`; after P5, P4a–P4d, K2: S1 `openai-chat-completions` custom profiles end to end with `provider check`; S2 setup form and recipe (after provider-setup-ui S3 and U1h–U1k) |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
