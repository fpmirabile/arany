# Continue with Tool failure and decline feedback

## Metadata

- Status: `Ready` (remaining C1–C2; prior-Run cause reinjection is delivered by the [Session contract](../../docs/specs/session.md#context-and-compaction))
- Owner: Fernando Mirabile
- Last updated: `2026-10-22`
- Next action: implement C1 after [approvals wait A1](../approvals-wait/README.md#vertical-slices), then C2. [Budget feedback](../budget-feedback/README.md) is independent.

## Outcome and scope

A recoverably failed or declined Tool reaches its requesting model as a specific correlated error result, so it can explain the problem and continue permitted work without another user submission. This plan owns that model-continuation feature across primary and delegated Tools. [Budget feedback](../budget-feedback/README.md) separately owns deterministic numeric limit explanations; [approvals wait](../approvals-wait/README.md) owns approval lifetimes and the cancel-versus-decline controls. Cancellation, Uncertain effects and inadmissible continuation still stop, with bounded local feedback rather than an unauthorized explanation call.

## Research

Inspected checkout: `cb87fa2cdd97fbfc2af06f91ebdfcec321f90bf0`. The owner's quoted 2026-10-22 session proves missing feedback, not the underlying cause of its Tool failures. No private State was inspected. The budget plan owns reproduction/classification of the generic budget-stop path; no actual incident cause is claimed here.

### Arany's existing path

| Finding | Evidence and implementation owner |
|---|---|
| Committed observations already distinguish success, failure, denial, conflict, limit, cancellation and uncertainty. `limit_cause` distinguishes timeout, output ceiling, context, read capacity, requested time and resource. Some output is still generic: `Tool result or resource limit reached`. | [Tools failure contract](../../docs/specs/tools.md#limits-and-failure-behavior), [dispatch mapping](../../crates/arany/src/tools/dispatch.rs), [ToolObservation](../../crates/arany/src/tools/types.rs), around 1101. |
| Ordinary attested failures can continue; explicit human approval refusal sets a stop cause in `decide_dispatch`/`refusal_cause`. This is not simply missing stderr. Some denial paths already continue, so they must not be duplicated. | [Shared step](../../crates/arany/src/engine/run_loop/step.rs), around 2347–2468 and `not_dispatched`; [primary decisions](../../crates/arany/src/engine/run_loop.rs), `decide_primary_call`; [Tools approval contract](../../docs/specs/tools.md#folder-trust-and-approvals). |
| `ApprovalReply::Deny`/boolean adapters currently conflate human No with a dropped reply or elapsed timer. The shared step emits `Approval was not granted; no operation was dispatched`. | [Approval channel](../../crates/arany/src/tools/approval.rs), [human/delegated resolution](../../crates/arany/src/engine/run_loop/step.rs), `human_reply`, `resolve_ask`, `not_dispatched`. Plan A removes the elapsed-human-wait case. |
| Each actual Provider call must be reserved, charged and journaled; exhaustion closes the Goal. An exhausted Goal cannot reopen. Existing bounded Goal reports prioritize failures/unknown outcomes before positive evidence. | [Goals lifecycle and report bounds](../../docs/specs/goals.md#lifecycle-budgets-and-recovery), [budget continuation](../../docs/specs/goals.md#budget-continuation), [Engine report](../../crates/arany/src/engine/report.rs). |
| Native adapters already pair model calls/results with request-local correlation; canonical observations retain enforcement evidence while model projections omit authority metadata. | [Provider encoding](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding), [architecture projection](../../docs/architecture/system-overview.md#1-what-the-beta-must-prove), [render corpus](../../crates/arany/src/provider/render/tests.rs). Reuse these paths. |
| Researcher failure can fail the Run; editor failure is generally local, with primary coordination and verification rules. Private Uncertain stops that actor; unknown Workspace publication stops the Run. | [Execution collaboration](../../docs/specs/execution.md#collaboration), [worker journal protocol](../../docs/specs/execution.md#worker-journal-protocol), [worker accounting](../../docs/specs/goals.md#worker-accounting-and-verification). |

### Peer behavior

Sources inspected 2026-10-22; Codex pin `61f34c35af77777db4d77d0ea2aeb8547a6b4523`, Claude changelog pin `5ed799e5cc56bea9efff57398ca43e31d82784ff`.

| Source | Verified behavior and decision relevance |
|---|---|
| [Codex approval conversion][codex-denial] and [correlated-result assertion][codex-result] | Denied becomes `ToolError::Rejected(reason)`; Abort becomes `TurnAborted`. Tests inspect the following Provider request's `function_call_output`, matched by call ID, for the rejection reason. [App-server approvals][codex-api] distinguish decline from cancel. Follow this for Arany's ordinary decline. |
| [Claude approval comments][claude-denial] | Reverified 2026-10-22 at the linked exact anchor: “If you select No without a comment on a prompt from the main conversation, Claude Code stops the turn.” A comment instead permits continuation. Thus peers share failure feedback but do not share unconditional continuation after every negative gesture. The owner explicitly requests an explanation without having to submit again; choose Codex's decline behavior, retaining a separate cancel. |
| [Claude interruption hint][claude-hint] | The pinned changelog records “What should Claude do instead?” as an interruption hint. It supports actionable redirection, not a mandatory extra Provider call after interruption. |
| [Claude Tool failures][claude-errors] and [permission decisions][claude-hook] | Failure text generally matches the Tool error given to Claude; validation errors become `tool_use_error`. A permission denial message is distinct from an explicit interrupt. Preserve reasons in correlated results rather than treating error text as an assistant answer. |
| [Claude headless permissions][claude-headless] | Approval-required calls without an answering channel/rule are denied. Explain an unavailable channel as such; never call it human rejection. |

No source establishes permission to spend past Arany's hard Goal budget. [Goals](../../docs/specs/goals.md#lifecycle-budgets-and-recovery) decides the exhaustion path: deterministic feedback, zero extra Provider calls. This is a guarantee-driven limit on peer-style model continuation, not an implicit budget exception. There is no need to invent a secondary-harness default where Codex/Claude already answer the behavior question.

Working rules: [RPI](../PLANNING.md), [security pass](../../agents/security.md#security-pass-triggers), [spec workflow](../../docs/specs/README.md), [testing/evidence owners](../../agents/testing.md), [Rust practice](../../agents/skills/rust-clean-code/SKILL.md), [architecture review](../../agents/skills/clean-architecture-review/SKILL.md). Ponytail favors the existing Tool result, report and stop projection; no explanation agent, logging service or second transcript store.

## Chosen direction

All decisions below were made **2026-10-22**.

| Decision | Basis |
|---|---|
| **C-D1:** Display the specific Tool/actor/cause from acknowledged facts and deliver a known recoverable failure or explicit decline exactly once as the correlated Tool result. Continue only through ordinary admitted model calls; tell the model to explain the failure and seek direction when needed. | Owner; [Codex denial/result][codex-result], [Claude failure results][claude-errors]. Claude's plain-No behavior differs as quoted above; follow Codex's separate decline/cancel behavior. |
| **C-D2:** No/Decline is not cancellation. Ctrl+C, approval Escape, pause, channel loss, hook stop and Uncertain effects stop their continuation; no automatic retry, re-approval or permission widening. Preserve private-worker uncertainty scope. | [Codex abort distinction][codex-denial], [approvals A1](../approvals-wait/README.md#chosen-direction), [guarantees](../../AGENTS.md#non-negotiable-guarantees). |
| **C-D3:** All continuation calls spend normal Goal budget. Exhaustion, context/storage failure or terminal safety limits use local feedback, never an extra paid explanation. Numeric budget records/projection belong solely to the independent budget plan. | [Goals](../../docs/specs/goals.md#lifecycle-budgets-and-recovery), [budget feedback](../budget-feedback/README.md). |
| **C-D4:** Hook/client denial reasons and rule text are not raw model input. Map denial source to compiled labels, applying the same escaping and size cap as UI feedback. Error excerpts remain separately bounded untrusted Tool-result data, never instructions or grants. | Owner review; [safe AI cage](../../AGENTS.md#non-negotiable-guarantees), [security rules](../../agents/security.md). |
| **C-D5:** Reuse native result encoding, existing actor coordination and bounded reports. No new Provider, retry mechanism, setting, dependency or legacy-default work; changed durable cause fields follow payload-version/fixture discipline. | Owner; [system ownership](../../docs/architecture/system-overview.md#2-runtime-architecture), [format discipline](../../AGENTS.md#global-rules). |

### Feedback contract

Engine/Tool boundaries retain typed causes until presentation. Extend existing permission/observation/terminal facts only where they currently erase a distinction: explicit decline, unavailable/disconnected channel, admission/drift failure, or an unsent request. The independent budget plan owns quantitative reservation records. The reducer owns valid cause/disposition combinations. Do not infer them by parsing human error prose. Presentation receives a shared semantic projection from Session/Goal/Run facts, rather than reimplementing budget arithmetic in every adapter. Reuse the existing in-process interface; wire types stay at serve/ACP/Provider edges.

| Situation | User-facing facts | Model/continuation |
|---|---|---|
| Command exits nonzero, filesystem conflict, MCP error or known Tool limit | Tool name and actor; exit code, conflicted resource or named limit; bounded escaped stderr/output excerpt only from the already admitted result; known effect/publication status. | Deliver once as the matching Tool result. Explain/recover within ordinary grants and remaining budget. No guaranteed-success claim about model prose. |
| Human decline / permission or hook deny | Exact source: user declined, rule/mode denied, hook denied; operation not dispatched only when proven. Use stable rule/cause labels without disclosing rule text or hidden paths. | Recoverable denial reaches the requesting actor, and existing coordination carries child results to the primary. Explain the refusal; no automatic retry of the refused effect. |
| Missing channel / malformed response / disconnect | Distinguish unavailable channel, invalid reply and connection loss from human No; state non-dispatch where established. | An undelivered request is `channel_unavailable`, never Decline; only a received correlated decline response is human rejection. A normal unavailable/invalid approval refusal may be explained through an admissible correlated result; actual connection/turn shutdown cancels and permits no new call. |
| Cancelled or Uncertain | Cancel source, stopped Tool and completed earlier work; Uncertain explicitly says the effect outcome is unknown and was not retried. | No continuation for the stopped scope. Never claim no effect after an unconfirmed dispatch. |

The [budget plan](../budget-feedback/README.md#durable-refusal-record-and-ordering) owns dimension selection, refused quantities and pre-commit versus durable limit feedback. C1/C2 consume it when available and preserve existing budget-stop behavior otherwise; they never invent a refusal snapshot or spend beyond a finite ceiling.

**Denial sanitization:** [step.rs](../../crates/arany/src/engine/run_loop/step.rs), `hook_denied`/denial formatting around 2582–2594, can currently interpolate a hook's reason. Before adding model continuation, project permission denial through one compiled vocabulary: `user_declined`, `permission_rule_denied`, `permission_mode_denied`, `hook_denied`, `channel_unavailable`, `invalid_approval_reply`, `cancelled`, `delegation_limit` or `goal_limit`, with a fixed explanation. No arbitrary hook message, client RPC error/result prose or permission-rule text is copied into that model result, including through historical Tool observations or compaction. The UI uses the same safe cause projection. Cap each escaped denial explanation at 2 KiB UTF-8 including any truncation marker, and apply the existing control/bidi escaping before both UI and model projection; structured fields stay within the same aggregate cap. Ordinary admitted command/MCP error excerpts are a separate Tool-output path with existing byte caps, inert framing and credential checks. Tests inject hostile hook/client/rule reasons and prove their raw bytes absent from Provider requests, rendered summaries and compaction while the compiled cause remains useful.

The normal transcript/Tool summary must expose the reason without `/agents`. Final failure feedback includes the terminal cause and up to eight recent failed/denied/limited Tool facts across primary and children, most recent first with actor labels and an omission count; uncertainty and the blocking cause have priority. Reuse the Goal report's 8 KiB total and 2 KiB per-item bounds, including escaped excerpts, rather than retaining another raw-output buffer. `show`, exec text/JSONL, terminal, serve and ACP derive equivalent facts from committed history. Protocols keep existing status/stop semantics; add bounded structured detail at their edge where needed. Local deterministic feedback is labelled an Arany status/report, never fabricated `MessageCommitted` model prose. Raw JSONL remains canonical Events.

Linux/macOS use the same feedback policy and current adapters. Native exit, signal and Guard distinctions remain platform-owned; no new OS API or support claim.

## Contract and scenarios

Owning specs: [Tools](../../docs/specs/tools.md#limits-and-failure-behavior) for reason/result semantics; [Execution](../../docs/specs/execution.md#accepted-outcomes) for continuation and terminal precedence; [Goals](../../docs/specs/goals.md#lifecycle-budgets-and-recovery) for charges and reports; [Provider](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding) for correlated errors; [Terminal](../../docs/specs/terminal.md#conversation-progress-and-history), [Serve](../../docs/specs/serve.md) and [ACP](../../docs/specs/acp.md) for rendering/protocol projection. Update each only in its delivering slice.

| Scenario | Required observation and evidence owner |
|---|---|
| Failed Tool then answer | Scripted Provider receives exactly the failed result with its call correlation and reason, then commits an explanatory answer; user sees the same Tool cause. [Engine corpus](../../crates/arany/tests/engine_run.rs), [native renderer corpus](../../crates/arany/src/provider/render/tests.rs), [Tool journey](../../crates/arany-cli/tests/session_run/tools.rs). |
| Explicit No versus cancel | No dispatch; No reaches an admitted continuation and answer without another user submission. Ctrl+C, approval Escape, protocol cancel and disconnect produce no subsequent Provider reservation. Explicit No/Decline remains separate from Escape, following Plan A and Codex. [Inline PTY](../../crates/arany-cli/tests/session_run/tools/inline_approval.rs), [serve](../../crates/arany-cli/tests/serve/in_process.rs), [ACP](../../crates/arany-cli/tests/acp/in_process.rs), [Guard permissions](../../crates/arany-cli/tests/guard_runs/permissions.rs). |
| Headless, child and host failures | Missing human channel is explicit; child result reaches its permitted parent path; no added child authority, discarded private work stays unpublished, host unknown remains unknown. [Worker journey](../../crates/arany-cli/tests/guard_runs/worker.rs), [unguarded journey](../../crates/arany-cli/tests/guard_runs/unguarded.rs), PR #279's host receipt/approval corpus when host A3 lands; it does not gate C1. |
| Hard stop, crash, storage refusal, Uncertain | No resumed effect or explanation call across the prohibited boundary; a lost acknowledgement is never reported as committed. Replay reproduces admitted details without Provider/Tool work. [Tool replay](../../crates/arany-cli/tests/session_run/tools/replay.rs), [reducer](../../crates/arany/src/session/reducer/tests.rs), [worker reducer](../../crates/arany/src/session/reducer/worker_tests.rs), Store fault owners. |
| Malicious/oversized error text and queue loss | Controls/bidi/markup inert, secret canaries absent, excerpts/aggregate limits enforced with omission notice; tap gaps recover from committed facts. Exact text/JSONL and schema-valid serve/ACP agree. Existing presentation, Provider render and protocol corpora own this. |

### Exact contract passages changed

C1 replaces [Tools approvals:39](../../docs/specs/tools.md#folder-trust-and-approvals), “Denial stops the Run without retry or rollback of completed actions”, with the explicit decline/cancel distinction. Reconcile Tools permission feedback at 53 and hook-denial output at 171; the failure/continuation rules under [Limits and failure behavior](../../docs/specs/tools.md#limits-and-failure-behavior) must identify recoverable results versus hard stops. [Execution accepted outcomes:139](../../docs/specs/execution.md#accepted-outcomes) currently includes a denied/dropped Tool step under `tool_stopped`; refine that classification and its reducer rules without weakening cancellation or uncertainty at 211. C2 reconciles [Execution worker journal protocol:85/97](../../docs/specs/execution.md#worker-journal-protocol): the broad denied-approval actor-stop sentence and the existing command-denial continuation exception must describe the chosen actor behavior consistently. Researchers retain read-only authority and established terminal failure propagation; stopped editors are reported through the primary, never revived. Provider/result and terminal/protocol specs change with each delivering slice, including denial sanitization and undelivered-request labels.

## Security and resource model

The security pass covers prompt assembly, authorization outcomes, durable facts, cost and untrusted rendering. Assets are private Workspace output, credentials, remote effects, journal integrity and paid usage. Tool stderr, MCP bodies, repository paths, client messages and model prose are untrusted data. Raw hook/client denial prose and rule text are excluded by the compiled-label projection above. Sources of authority remain pinned user settings/grants and explicit approval; a reason or suggested remedy grants nothing. Startup/egress admission remains unchanged. Hook/review restrictions and descendant intersections still narrow only.

Use existing request, Tool-result, context, Event/Store and output-queue bounds. Summary overflow drops lower-priority details with a count, never uncertainty or the terminal cause. Local fallback performs no filesystem rediscovery, secret lookup or network call. No raw Provider bodies, credentials, environment dumps or hidden paths enter summaries or telemetry. Model guidance is compiled; error excerpts remain Tool-result data, never instructions. Reserve normal action/terminal headroom before work; no “last explanation” exception after budget refusal. A full client queue cannot delay journal settlement/cancellation. No live-provider quality or native-support claim follows from synthetic tests.

## Risks and mitigations

- Generic errors erase the actionable cause → retain typed boundary facts and values; do not guess from prose or old counts.
- Recoverable decline accidentally becomes an execution retry → require a fresh model proposal, normal permission checks and a new intent; replay never redispatches.
- An answer hides incomplete work → keep Goal success criteria and worker verification authoritative; a useful explanatory answer may accompany a failed Goal.
- Error projection leaks data or grows context → shared admitted output, explicit byte/item caps, hostile-text/canary scenarios and no new raw-output store.

## Vertical slices

1. **C1 — A declined or recoverably failed primary Tool reaches the model with a safe reason and admitted continuation.** After approvals A1, one PR crosses typed terminal/protocol reply → permission verdict/Tool observation → sanitized native correlated result → ordinary Engine continuation → committed answer or bounded local fallback → client views, specs and existing PTY/Engine/Guard/renderer evidence. Prove explicit decline versus Escape/Ctrl+C/cancel, unavailable/unsent delivery, hostile hook/client/rule reasons, nonzero exit, conflict, failure then Provider error and exact-budget refusal with no extra call. This works without the independent numeric budget-feedback feature.
2. **C2 — Delegated Tool failures reach their requesting actor and primary through existing coordination.** One PR crosses researcher/editor permission/result → permitted child continuation or terminal actor cleanup/journal → primary coordination → explanatory answer/local fallback → all client history, worker specs and corpora. Keep actor Tool/turn/headroom limits, failure propagation and private/Workspace uncertainty distinctions. Verify failed children with healthy siblings, denied integration, sanitized child reasons and insufficient primary continuation budget; no stopped actor is revived for an explanation.

## Exit criteria

- **Research gate passed:** current loss points, budget/uncertainty rules, and peer decline/error/cancel distinctions have primary evidence. The actual dogfood Tool causes remain unclaimed and are not required to implement feedback.
- **Plan gate passed:** all continuation choices are decided; no owner-only question remains. Budget explanation and rewind confirmation have separate plans. No legacy-default preservation work, new dependency or budget exception is proposed.
- **Implement gate:** each slice demonstrates exact visible reasons, model request correlation, admitted call counts, closed-store Events and reduced views through existing owners. Exhausted/cancelled/uncertain paths make zero extra calls; numeric stop projection is verified in its own plan. Formatting, Clippy, targeted tests and full CI belong to implementation; no Cargo command runs for this plan update.
- Synthetic proof does not establish model quality, real-account behavior or Linux/macOS terminal fidelity. Track genuinely deferred native/live checks under the existing [NEXT_STEPS](../../NEXT_STEPS.md) owners when implementing. Close the plan/index row under the [planning lifecycle](../README.md#lifecycle) once its behavior lives in the owning specs.

[codex-denial]: https://github.com/openai/codex/blob/61f34c35af77777db4d77d0ea2aeb8547a6b4523/codex-rs/core/src/tools/approvals.rs#L418-L448
[codex-result]: https://github.com/openai/codex/blob/61f34c35af77777db4d77d0ea2aeb8547a6b4523/codex-rs/core/tests/suite/approvals.rs#L2129-L2140
[codex-api]: https://developers.openai.com/codex/app-server#approvals
[claude-denial]: https://code.claude.com/docs/en/permissions#add-a-comment-when-you-answer-a-permission-prompt
[claude-hint]: https://github.com/anthropics/claude-code/blob/5ed799e5cc56bea9efff57398ca43e31d82784ff/CHANGELOG.md#L1261
[claude-errors]: https://code.claude.com/docs/en/hooks#posttoolusefailure
[claude-hook]: https://code.claude.com/docs/en/hooks#permissionrequest-decision-control
[claude-headless]: https://code.claude.com/docs/en/permission-modes#tool-calls-denied-in-claude--p-and-agent-sdk-runs
