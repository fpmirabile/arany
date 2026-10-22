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
| [Approvals wait for the human](./approvals-wait/README.md) | `Ready`: A1 non-host primary approvals now; A2 transition-driven child wait accounting; A3 host expiry after #279 |
| [Rewind confirmation](./rewind-confirmation/README.md) | `Ready`: R1 live preview confirmation without wall-clock expiry, after approvals A1 |
| [Budget feedback](./budget-feedback/README.md) | `Ready`: G1 reproduce/classify budget refusal, persist its reservation detail and render deterministic explanations; independent of approvals |
| [Tool failure continuation](./failure-feedback/README.md) | `Ready`: C1 primary failure/decline results and sanitized continuation after approvals A1; C2 delegated feedback |
| [Arany-owned storage report](./arany-storage/README.md) | `Ready`: independent two-slice feature for user-local aggregated sizes/counts/paths via `arany storage`/`/storage`, then recorded native Guard storage; metadata-only lead default, no automatic model disclosure |
| [Provider check journal](./provider-check-journal/README.md) | `Ready`: C1 standalone custom/native check streams and SQLite migration; C2 ChatGPT checks and account-index migration; prerequisite for setup UI |
| [Provider setup UI](./provider-setup-ui/README.md) | `Ready`: depends on Provider check journal C1–C2; three PRs for account/custom-route management, per-role routing and named roster editing in Fullscreen, Classic and linear modes |
| [MultiEdit](./multiedit/README.md) | `Ready`: one slice, atomic ordered edits to one existing file; MultiEdit freeze exception accepted |
| [Large draft attachments](./large-draft-attachments/README.md) | In progress: pasted-text slice implemented; typed/restored drafts and clipboard/path image chips remain |
| [Composer newline keys](./composer-newline-keys/README.md) | Ready: two PRs for portable newline entry and saved shortcuts, then negotiated modifier keys with capability-aware guidance |
| [Git write workflow](./git-write-workflow/README.md) | Three slices: guarded local Git writes, user-owned typed prefix rules, human-approved host push and PR creation |
| [Team supervision](./agentic-teams/README.md) | Calibrated signal enforcement, read-only changeset review and one-use user responses; shadow evaluation shipped |
| [Persistent worker assignment views](./worker-assignment-views/README.md) | Future C3 assignment-long Guard views, read-only assignment-time cache and MCP invocation exclusion; admission gate remains |
| [Goal RPI](./goal-rpi/README.md) | Proposed durable phases, plan gate, bounded grilling, phase context and explicit headless RPI |
| [Goal activity presentation](./goal-progress/README.md) | Proposed bounded agent notes and Goal activity/usage/question views |
| [Per-agent route recovery](./agent-route-recovery/README.md) | Deferred exhausted-route pause, dependency blocking and user retry/switch |
| [Typed judge adapters](./judge-adapters/README.md) | Deferred API research and one admitted typed-judge capability |
| [Public-registry fetch](./registry-fetch/README.md) | Frozen proxy-only dependency retrieval followed by offline builds |
| [Git branch reads](./git-branch-reads/README.md) | Ready; S1 local/remote-tracking branch metadata and divergence, S2 bounded range history and endpoint diffs |
| [GitHub PR reads](./github-pr-reads/README.md) | Ready; depends on Git-write S3; S1 host PR listing with human or user-owned typed-prefix approval, S2 bounded PR discussion and checks |
| [Primary host commands](./primary-host-commands/README.md) | `Ready`: guarantee wording and H1 approved; S1 primary escalation, session approval and privilege-tool refusal, S2 user-owned host prefixes, S3 serve/ACP/exec |
| [Real-scale command snapshot](./real-scale-command-snapshot/README.md) | Ready: three command slices; `ignore` 0.4.33 approved behind the dependency review gate |
| [Gitignore-aware read discovery](./gitignore-read-discovery/README.md) | Ready: typed discovery/suggestions, then Git disclosure; depends on snapshot PR1 (selector and enforced preparation) |
| [Agentic coding teams](./agentic-teams/README.md) | C2 edit-only workers, C3 command-capable views, roster presets, RPI, Supervisor adapters and deferred Goal/fetch/Git workflows |
| [Linux end-to-end Tool latency](./linux-tool-latency/README.md) | Read, Edit and guarded-command p95 reporting through durable result acknowledgement; ignored release corpus and owner-run named-host evidence |
| [Guarded Cargo tests](./guarded-cargo-test/README.md) | Ready: selected offline Rust environment, Guard-only IPC test ignores, bounded multicore Build profile and exact repository measurement; owner decisions accepted |

[Next steps](../NEXT_STEPS.md) owns priority, freezes and deferred checks.
