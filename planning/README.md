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
| [Architecture homogenization](./architecture-homogenization/README.md) | `Ready`: 54 behavior-preserving implementation slices in 16 tracks plus one terminal lifecycle research slice (55 slices / 17 tracks); bounded flows, complete runtime/decoder ownership and live-worktree gates, four independent owner decisions |
| [Approvals wait for the human](./approvals-wait/README.md) | `In progress`: A1 non-host primary approvals; A2 transition-driven child wait accounting; A3 host expiry after #279 |
| [Tool failure continuation](./failure-feedback/README.md) | `Ready`: C1 same-Run primary failure/decline results and sanitized continuation after approvals A1; C2 delegated feedback; prior-Run cause reinjection is owned by the Session contract |
| [Arany-owned storage report](./arany-storage/README.md) | `In progress`: S2 recorded native Guard storage remains; S1 local StateRoot/cache report uses the metadata-only lead default, with no automatic model disclosure |
| [Provider check journal](./provider-check-journal/README.md) | `In progress`: C1 implemented; C2 selected ChatGPT checks and account-index migration remain; prerequisite for setup UI |
| [Provider setup UI](./provider-setup-ui/README.md) | `Ready`: depends on Provider check journal C1–C2; three PRs for account/custom-route management, per-role routing and named roster editing in Fullscreen, Classic and linear modes |
| [Anthropic Tool turns](./anthropic-tool-stream/README.md) | `Ready`: S1 names Anthropic failure stages in `development.log`; S2 journals the closed reason; S3 fixes the cause after the owner's live re-run |
| [Route output budget](./route-output-budget/README.md) | `Ready`: 1 raises the primary output cap to 32,768 tokens on ChatGPT and native OpenAI/Anthropic so 64 KiB Write and Edit fit; 2 gives private editor children the same cap |
| [Large draft attachments](./large-draft-attachments/README.md) | In progress: pasted-text slice implemented; typed/restored drafts and clipboard/path image chips remain |
| [Composer newline keys](./composer-newline-keys/README.md) | In progress: remaining slice negotiates modifier keys with capability-aware guidance and terminal restoration |
| [Team supervision](./agentic-teams/README.md) | Calibrated signal enforcement, read-only changeset review and one-use user responses; shadow evaluation shipped |
| [Persistent worker assignment views](./worker-assignment-views/README.md) | Future C3 assignment-long Guard views, read-only assignment-time cache and MCP invocation exclusion; admission gate remains |
| [Goal RPI](./goal-rpi/README.md) | Proposed durable phases, plan gate, bounded grilling, phase context and explicit headless RPI |
| [Goal activity presentation](./goal-progress/README.md) | Proposed bounded agent notes and Goal activity/usage/question views |
| [Terminal usage view](./usage-view/README.md) | `In progress`: U1 and U2 implemented; next U3 API/custom rate limits |
| [Per-agent route recovery](./agent-route-recovery/README.md) | Deferred exhausted-route pause, dependency blocking and user retry/switch |
| [Typed judge adapters](./judge-adapters/README.md) | Deferred API research and one admitted typed-judge capability |
| [Public-registry fetch](./registry-fetch/README.md) | Frozen proxy-only dependency retrieval followed by offline builds |
| [Git branch reads](./git-branch-reads/README.md) | In progress; guarded S2 range/diff evidence added, full test suite and CI remain |
| [GitHub PR reads](./github-pr-reads/README.md) | Ready; depends on Git-write S3; S1 host PR listing with human or user-owned typed-prefix approval, S2 bounded PR discussion and checks |
| [Primary host commands](./primary-host-commands/README.md) | `Ready`: guarantee wording and H1 approved; S1 primary escalation, session approval and privilege-tool refusal, S2 user-owned host prefixes, S3 serve/ACP/exec |
| [Real-scale command snapshot](./real-scale-command-snapshot/README.md) | Ready: three command slices; `ignore` 0.4.33 approved behind the dependency review gate |
| [Gitignore-aware read discovery](./gitignore-read-discovery/README.md) | In progress: typed discovery/local suggestions await CI/native evidence; Git disclosure remains |
| [Agentic coding teams](./agentic-teams/README.md) | C2 edit-only workers, C3 command-capable views, roster presets, RPI, Supervisor adapters and deferred Goal/fetch/Git workflows |
| [Linux Tool latency](./linux-tool-latency/README.md) | `In progress`: slice 2 extends the replay-audited Read report with guarded Edit and command rows; named-host evidence remains owner-run |
| [Guarded Cargo tests](./guarded-cargo-test/README.md) | In progress: validate the one-job Rust slice; bounded multicore Build profile and exact repository measurement remain |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
