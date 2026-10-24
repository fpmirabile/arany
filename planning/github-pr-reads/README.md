# Read GitHub pull requests through the host execution class

## Metadata

- Status: `Ready` (Research and Plan gates passed; implementation not started).
- Owner: `fpmirabile`; the implementation lead owns slice delivery and evidence.
- Last updated: `2026-10-22`.
- Dependency: [Git write S3, landed](../../docs/specs/tools.md#human-approved-host-git), including its pinned host executor, credential admission, human approval and host receipt/replay validation. S2 supplies typed prefix machinery.
- Next action: after S3 lands or supplies the stacked base, implement S1. Ready describes the Plan gate, not dependency completion.

## Outcome and scope

One feature: the primary can list GitHub PRs and inspect a selected PR's description, review decision, comments and checks to answer questions about open work. Reuse the user's existing `gh` authentication and S3 host class; no Guard network proxy, new API client, token setup or dependency. [Typed Git reads](../../docs/specs/tools.md#typed-git-reads) provides independent local comparisons; this feature requires no local checkout/fetch of a PR.

No PR mutation, checkout, diff download, arbitrary `gh api`, browser launch, watch loop or child-agent capability. GitHub text and URLs remain untrusted data, never instructions, authorization or automatically followed links.

## Research

Sources checked on 2026-10-22; local baseline is `8d4281e3768142574b60e7051e9015969d29dc2e`.

| Finding | Source and consequence |
|---|---|
| Claude's own review workflow permits `gh pr list/view/diff` and reads comments. Its documented shell rules can remember permissions; excluded commands retain that permission flow outside the sandbox. | Pinned [review workflow](https://github.com/anthropics/claude-code/blob/5ed799e5cc56bea9efff57398ca43e31d82784ff/plugins/code-review/commands/code-review.md), official [permissions](https://code.claude.com/docs/en/permissions) and [sandbox exclusions](https://code.claude.com/docs/en/sandboxing#run-commands-outside-the-sandbox-with-excludedcommands). Copy PR-reading capability, not an imagined typed Claude API. |
| Codex documents `gh pr view` prefix rules; allow rules can permit execution outside the sandbox without another prompt. Its current permission prompt can request additional permissions or full escalation. | Official [rules](https://learn.chatgpt.com/docs/agent-configuration/rules), pinned [execpolicy](https://github.com/openai/codex/blob/86e17e98c7f901277aeeb4649cf1b01462b082f2/codex-rs/execpolicy/README.md) and [permission prompt](https://github.com/openai/codex/blob/86e17e98c7f901277aeeb4649cf1b01462b082f2/codex-rs/prompts/templates/permissions/approval_policy/on_request_rule_request_permission.md). Do not claim every Codex GitHub call escalates or asks. |
| Arany S2 rules already exist; S3 remains a dependency. Its write exception keeps push and PR creation always human-approved. The owner approved the explicit read extension below on 2026-10-22. | [Typed prefixes](../../docs/specs/tools.md#typed-prefix-rules), [S3 exception/auth decisions](../../docs/specs/tools.md#human-approved-host-git), [AGENTS guarantees](../../AGENTS.md#non-negotiable-guarantees). |
| `gh pr list` defaults to open/30, supports repository, base/head and bounded limit; list/view support structured JSON fields. | Official [list](https://cli.github.com/manual/gh_pr_list), [view](https://cli.github.com/manual/gh_pr_view); pinned [list implementation](https://github.com/cli/cli/blob/7948e8056e45ac3c729b829e0eafe2d0797a95c6/pkg/cmd/pr/list/list.go). Reuse fixed fields and these defaults. |
| View automatically retrieves all pages of comments, reviews and check rollups; checks does likewise. JSON checks return before the text-mode pending/failure exit handling. | Pinned [finder](https://github.com/cli/cli/blob/7948e8056e45ac3c729b829e0eafe2d0797a95c6/pkg/cmd/pr/shared/finder.go#L266-L285), [pagination loops](https://github.com/cli/cli/blob/7948e8056e45ac3c729b829e0eafe2d0797a95c6/pkg/cmd/pr/shared/finder.go#L444-L613), [checks](https://github.com/cli/cli/blob/7948e8056e45ac3c729b829e0eafe2d0797a95c6/pkg/cmd/pr/checks/checks.go). A stdout cap alone does not bound those collections; exit success does not mean checks passed. |
| The same installed gh supports a single non-paginated GraphQL request with JSON input. Its existing query builder supplies the PR/comments/reviews/check union shape. | Official [gh api](https://cli.github.com/manual/gh_api), pinned [query builder](https://github.com/cli/cli/blob/7948e8056e45ac3c729b829e0eafe2d0797a95c6/api/query_builder.go). Use that narrow adapter path for bounded connections, not a general model-facing API Tool. |

Research gate passed: peer permissions, current dependency, gh fields and its hidden pagination were checked. Plan gate passed: all material choices below are decided. Reopen Research if the S3 implementation or admitted gh version contradicts them.

## Chosen direction

All decisions below are **Decided 2026-10-22**.

| Decision | Basis |
|---|---|
| Offer primary-only `gh_pr_list {repository,state,base,head,limit}` and `gh_pr_view {repository,number}`. Null state/limit resolve to `open`/30; allowed states are `open`, `closed`, `merged`, `all`, and limit is 1–50. Base/head are optional exact names, not a search language. | Peer parity: gh list/view; owner scope. |
| **Decided 2026-10-22 (owner):** `gh_pr_list` and `gh_pr_view` ask by default in every mode that asks; a matching user-owned S2 typed prefix allow rule may pre-approve them. No repository-sourced authority, child inheritance or AI/hook-only approval. | Peer parity: [Claude permission allow rules](https://code.claude.com/docs/en/permissions#manage-permissions), pinned [Claude review workflow](https://github.com/anthropics/claude-code/blob/5ed799e5cc56bea9efff57398ca43e31d82784ff/plugins/code-review/commands/code-review.md) and [Codex execpolicy prefixes](https://github.com/openai/codex/blob/86e17e98c7f901277aeeb4649cf1b01462b082f2/codex-rs/execpolicy/README.md); owner-approved host exception below. |
| Reuse S2 canonical prefixes `gh pr list` and `gh pr view` for user-owned deny/ask/allow editing, remembering and provenance. Only these typed read prefixes can pre-approve host reads; ordinary allow rules and upfront grants cannot substitute. | Owner decision above; [S2 machinery](../../docs/specs/tools.md#typed-prefix-rules). Deny and explicit ask retain precedence; rules never widen the pinned grant. |
| Reuse S3's pinned gh, user-owned auth store, explicit repository/host and suppressed pager/alias/extension/editor. No repository discovery or project-supplied credentials. | Owner's S3 [decided defaults](../../docs/research/git-writes-2026-10.md); peer use of gh authentication. |
| List and scalar view use ordinary gh JSON; bounded comments/reviews/checks use one fixed GraphQL query through the same gh. | Guarantee-driven divergence from direct `gh pr view/checks` pagination: explicit resource bounds; pinned source above. No new client or dependency. |
| Treat partial, missing and stale status explicitly; preserve GitHub's `reviewDecision` and check state/conclusion, never infer approval or green checks from process success. | Peer parity: gh JSON field semantics and checks implementation; honest uncertainty. |

**Execution and fields.** Canonicalize repository as admitted host/owner/name before approval; use S3's destination admission, github.com default and user-configured alternative hosts. Model arguments and returned URLs cannot select an unadmitted host. Bind repository, operation, filters/number, gh digest, query/field-set version and limits to the intent. PR number is an integer 1–2,147,483,647; repository spelling is at most 256 bytes and base/head at most 128 bytes, control-free and validated before argv construction.

- List: fixed `gh pr list --repo HOST/OWNER/REPO --state ... --limit ... --json ...`, plus validated base/head filters. Fields: number, title, URL, author login, state, draft, base/head names and OIDs, reviewDecision and updatedAt. No body or nested connections. Reaching the requested limit means `possibly_more:true`; gh does not expose total count here. Zero rows is successful, not an authentication failure.
- View: fixed `gh pr view NUMBER --repo HOST/OWNER/REPO --json ...` with the same scalar fields plus body and mergeable; then one fixed GraphQL query for that repository/number, head OID, aggregate review decision, the latest 50 comments, latest 50 reviews, and at most 100 check/status contexts on the latest commit. Copy the union field selection from the pinned query builder; retain author, timestamp, body/state and check name/status/conclusion, not arbitrary API fields. `last` connections report `hasPreviousPage`; `first` contexts report `hasNextPage`. Inline review threads and full review history are outside this bounded view.

The second command is fixed `gh api graphql --hostname HOST --method POST --input -`, with an adapter-generated JSON body on stdin and explicit owner/name/number variables. It has no `--repo`; variables provide the equivalent repository pin. No caller query, endpoint, headers, `--paginate`, interpolation or `--field` file-loading syntax is accepted. Both view commands belong to the immutable read and one journaled human or typed-prefix-rule verdict; no mutation operation exists in either command. A new head OID between replies marks the composite stale and preserves each observation's identity; it never combines old-head details with new-head checks as one current snapshot or refreshes automatically.

**Exact planned AGENTS.md amendment — owner-approved 2026-10-22.** In S1, replace S3's exception paragraph in the Per-effect isolation bullet with the following and update Tools/Execution together:

> The exceptions are: a per-command run outside the Guard that the user approves after a sandbox-caused failure, never automatic, journaled and marked unguarded; and typed `git_push`, `gh pr create`, `gh_pr_list` and `gh_pr_view` operations on the host with inert Git configuration. Host `git_push` and `gh pr create` always require human approval, including in Auto, and are refused when no human approval channel is available. Host `gh_pr_list` and `gh_pr_view` ask by default in every mode that asks and may be pre-approved only by a matching user-owned typed prefix allow rule; they are refused when neither a human approval channel nor such a rule exists. Repository content supplies no authority, children do not inherit these host-read approvals, and AI or hook approval alone cannot authorize host reads. All these typed host operations are journaled as a separate execution class and marked host. Other guarantees still apply.

This plan changes no runtime authority. S1 implements list; view is not offered or admitted until S2. The read exception does not change S3's mandatory per-call human approval for writes, and every read still requires its pinned grant, schema and destination admission.

| Effective permission/channel | Read disposition |
|---|---|
| `default`, `acceptEdits`, `auto` | Matching user-owned typed allow pre-approves; otherwise ask a human. Auto cannot substitute AI approval. |
| `plan` | Typed reads are eligible with the same typed-allow-or-human decision; no generic host process becomes eligible. |
| `dontAsk`, no human channel, or ACP without human attestation | Proceed only with a matching user-owned typed allow and no overriding ask/deny; otherwise refuse. ACP client policy is not a user-owned Arany rule. |
| Explicit deny, cancellation, expired intent or dropped approval | No dispatch. Deny beats ask beats allow; an explicit ask requires a human or refuses. Hooks/reviews can only narrow. |

An attested human-capable serve client may approve as S3 permits. Reuse S2's user-owned settings/remember flow, atomic saves, folder identity and caps; a remember response approves the current call once, while its saved rule applies at next Run admission. Rule edits cannot alter an admitted Run. Journal the exact intent, approval source (`human` or `typed_prefix_rule`), pinned settings digest and matched rule provenance; replay validates them without loading current settings. Rules authorize only the primary and never reach children. Approval and rule-edit surfaces disclose host execution and private PR disclosure to the current Provider. Credentials stay at the host edge; neither Tool result nor Provider input/journal contains tokens, auth files, ambient environment or raw credential diagnostics.

**Ownership/platforms.** Engine owns policy, Goal accounting and journal-first ordering; Tools owns normalized operations, repository/gh pins, closed argv/query construction and result translation; S3's host supervisor owns dispatch, deadline and cleanup; reducer/Store own verdict/receipt validation and replay; CLI owns approval and committed `host` presentation. Extend these concrete interfaces, with no parallel executor trait or network service. GitHub JSON remains at the adapter edge. Linux/macOS use the same semantics and their S3 native admission/auth paths; unavailable enforcement/admission refuses. No native/live support is claimed by the plan.

## Security and resource model

The [security pass](../../agents/security.md#security-pass-triggers) covers host execution, network/auth, permissions, untrusted remote content and persisted output. Assets are user credentials, private PR data, host files and canonical history. Startup admits user configuration, executable/auth identity and destination before consuming model or repository suggestions. Returned titles, bodies, comments, reviews, check names and URLs are escaped, bounded historical data; embedded instructions cannot invoke Tools, alter permissions, change the destination or obtain credentials. The human approval or user-owned rule flow makes private PR disclosure to the current Provider visible.

Use existing Tool/result/Store budgets, one host read in flight and no new queue/cache. Per typed read: at most two sequential gh processes, one shared 20-second deadline narrowed by Goal allowance, 256 KiB captured stdout per process, 4 KiB stderr and at most 1 KiB sanitized diagnostics. The fixed supplemental query is at most 8 KiB including variables. No pagination or Arany-level automatic retry; failures/rate limits return once. Keep at most 50 list rows or 200 connection rows; clip body/comment/review text to 8 KiB each, title to 256 bytes and other strings to 2 KiB, with field/result truncation flags and the final Tool budget taking precedence. Invalid or capture-overflow JSON fails/limits rather than publishing a partial parsed success; GraphQL errors are reported even with HTTP/process success. Unavailable/null status is unknown, empty checks means none reported, and truncated connections cannot prove an exhaustive review.

S3's host limitations remain explicit: no Guard filesystem/network/CPU/memory/process-count isolation, DNS/redirect confinement or guaranteed containment of escaped descendants. Fixed query counts bound requested connection rows and avoid pagination; they do not cap gh's internal HTTP response allocation, redirects or transport attempts. Time/output supervision is not a network-byte or host-memory cap. Preserve S3's closed environment, credential handling and kill/reap behavior; do not introduce a proxy or pretend host execution is Guard-attested.

Commit intent and the human or typed-prefix-rule verdict before the first host dispatch; commit the bounded receipt/observation before continuation. A confirmed auth/API/parse failure is Failed and never a zero-PR or approved result. An interrupted dispatched action without confirmed completion/cleanup is Uncertain, stops continuation and never retries itself. A later fresh user task can request another read. Replay/reopen uses committed facts only and never contacts GitHub, rereads credentials or launches gh.

## Contract and scenarios

[Tools](../../docs/specs/tools.md) owns schemas, host eligibility, approval/disclosure, bounds and outcomes; [Execution](../../docs/specs/execution.md#admission-persistence-and-recovery) owns ordering, host verdict/receipt validation and replay. Extend their S3 sections and acceptance rows; the [terminal contract](../../docs/specs/terminal.md) changes only if its S3 host projection needs new visible behavior.

| Scenario | Required observation and evidence owner |
|---|---|
| Open PR assessment | List default open/30, select a number, view description/review decision/checks; match requested repository and head identity in Provider result, terminal/exec/show, closed-store Events and reduced views. Extend S3 host product journeys in [session_run/tools.rs](../../crates/arany-cli/tests/session_run/tools.rs) and [guard_runs.rs](../../crates/arany-cli/tests/guard_runs.rs). |
| Rules/modes/roles | Asking modes prompt without a typed allow; a user-owned typed allow pre-approves, including plan/Auto and headless/ACP. Deny/explicit ask win; repository rules, ordinary allows, AI/hook-only approval, upfront grants and children cannot authorize dispatch. Test rule save/revoke/next-Run pinning and forged provenance; writes still require a human. Extend [permission tables](../../crates/arany/src/tools/permissions/tests.rs), S3 approval and [reducer tests](../../crates/arany/src/session/reducer/tests.rs). |
| Hostile routing/content | Config, aliases, extensions, editor/pager settings and PR instructions cannot execute code or redirect the admitted request. Credential canaries are absent across output, journal, Provider and diagnostics. Extend S3 supervised synthetic GitHub peer and host fixtures. |
| Partial/failing data | Test empty list, at-limit list, pending/failing/no checks, null review decision, GraphQL partial errors, denied/auth-expired/rate-limited responses, oversized data and head movement. Fixed query never requests another page. Extend adapter corpus and S3 product observations. |
| Crash/cancellation/replay | Denial before dispatch sends nothing; confirmed failure differs from unknown completion. Uncertain stops continuation, and replay after closing the Store launches no process/network request. Extend S3 product/reducer/Store crash fixtures. |

## Risks and mitigations

- A harmless remote verb may be mistaken for contained host execution → explicit human-or-user-rule authority for reads and `host` marking everywhere, including failures.
- gh pagination or successful exit can fabricate completeness/green status → fixed bounded queries, explicit truncation/null/error semantics and pinned-source fixtures.
- S3 may change during implementation → use its landed executor and evidence owners; reconcile this plan before coding if the boundary differs. Do not build a duplicate host path.

## Vertical slices

1. **S1 — List PRs with explicit host consent (one PR, on Git-write S3).** Land the narrow named exception and typed list schema/catalog/grant → repository normalization and S2 rule handling → journaled intent/human-or-typed-prefix-rule verdict → supervised pinned gh/auth → bounded committed host result → Provider continuation and terminal/exec/show. Deliver default/filter/limit/empty-list, typed-rule preapproval/revocation, mode/role refusal, hostile routing, credential-canary and replay scenarios. Update Tools/Execution/required guarantee wording in the same PR. Alone this answers which PRs are open and their review decisions.
2. **S2 — Inspect one PR's bounded discussion and checks (one PR, on S1).** Ship typed view end to end through the same admission/approval/journal/host path, scalar view plus fixed supplemental query, identity/truncation translation and visible host results. Deliver failed/pending/null/partial-status, head-race, hostile-content, timeout/cancellation and closed-replay scenarios; update Tools/Execution and schema snapshots. Alone this lets the primary explain what is awaiting review or failing, with explicit limits on the evidence.

## Exit criteria

- Success/refusal/failure/recovery scenarios pass through existing S3 owners and [ObservationBundle](../../agents/testing.md#user-visible-proof); synthetic peers must succeed if a forbidden request reaches them, so refusal assertions prove no dispatch.
- All mutations remain unavailable through these reads; forged human/rule provenance, AI/hook-only approval, child authority, wrong repository, wrong execution class or missing receipt is rejected. Replay validates committed typed-prefix verdicts without reauthorization and performs zero host/network/credential work.
- A stored version change ships the existing [migration contract's](../../docs/specs/execution.md#journal-migration) required step and fixture, with no compatibility reader.
- Implementation runs targeted formatting, Clippy/tests and full CI. This documentation update runs only prose/link/diff checks. Real GitHub/auth, Linux/macOS native teardown and human approval/accessibility checks get one owner/procedure in NEXT_STEPS when implementation lands; no such evidence is claimed now.
- Accepted behavior lives in the owning specs; close/remove this plan and its index row under the [planning lifecycle](../README.md#lifecycle).
