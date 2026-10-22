# Explain refused Goal budgets locally

## Metadata

- Status: `Ready`
- Owner: Fernando Mirabile
- Last updated: `2026-10-22`
- Next action: begin G1 with the incident-path reproduction/classification below; no dependency on approvals, Tool continuation or PR #277.

## Outcome and scope

When a Goal refuses its next action, the user sees the actual budget dimension, charged usage, pinned limit and any known required reservation, together with relevant earlier Tool failures. This deterministic explanation is usable without changing model continuation. [Tool failure continuation](../failure-feedback/README.md) owns that separate feature. No extra Provider call, retry, fallback or compaction is allowed after exhaustion.

## Research

Inspected revision: `cb87fa2cdd97fbfc2af06f91ebdfcec321f90bf0`. The owner's quoted incident does not supply the Run journal or numeric settings. It establishes generic feedback, not proof of actual exhaustion or the reason each Tool failed. Private State remains out of scope.

| Finding | Verified owner |
|---|---|
| `run_feedback` takes only a Run and sets `budget_stop` from `RunFailure::BudgetExhausted`; after other branches it emits generic budget copy at 1141–1146. `primary_tool_limit_reached()` has its own preceding branch. | [presentation.rs:1070–1150](../../crates/arany-cli/src/presentation.rs); [presentation model](../../crates/arany-cli/src/presentation/model.rs), around 942/1050. |
| A shared projection names tokens, calls, active time or Runs; unknown tokens count as unknown, not zero. Active time within the five-second Store margin can refuse below the nominal ceiling. A failed Provider call on its last allowed attempt is not necessarily exhaustion. | [session/stop.rs](../../crates/arany/src/session/stop.rs), `project`/`exhausted_limit`; [Goals lifecycle](../../docs/specs/goals.md#lifecycle-budgets-and-recovery); [Execution accepted outcomes](../../docs/specs/execution.md#accepted-outcomes). |
| `reserve_active` computes a requested interval, clamps to remaining time and rejects an insufficient minimum reservation. Per-action reservations live in memory; the journal retains action ceilings and Run-level settlement, not the rejected amount. | [ledger.rs:552–584](../../crates/arany/src/engine/ledger.rs); [Goals lifecycle](../../docs/specs/goals.md#lifecycle-budgets-and-recovery), paragraph beginning “Per-action reservations and settlements live in Engine memory”. |
| `RunFinished` has `failure` and `active_ms`; `GoalFinished` has status, active time, report and `exhausted`, but neither currently carries the refused reservation. | [events.rs:279–286,448–453](../../crates/arany/src/session/events.rs). A new field is proposed below, not claimed as existing evidence. |
| The report already prioritizes failure/uncertainty within 8 KiB total, 2 KiB per item and 32 items per list. Exhaustion closes the Goal, with explicit continuation creating a new Goal. | [Goals reports](../../docs/specs/goals.md#lifecycle-budgets-and-recovery), [budget continuation](../../docs/specs/goals.md#budget-continuation), [Engine report](../../crates/arany/src/engine/report.rs). |
| PR #277 changes new-Goal defaults, not finite selected budgets or finite action limits. | [PR #277](https://github.com/fpmirabile/arany/pull/277), [default plan](../attached-turn-defaults/README.md). No dependency or default conversion here. |

### Incident-path classification before implementation

**First work in G1:** reproduce the quoted sequence synthetically at the existing [Engine Goal corpus](../../crates/arany/tests/engine_run/goals.rs) and [Tool/terminal journey](../../crates/arany-cli/tests/session_run/tools.rs): known Tool failures, followed by an attempted next action, then the generic presentation branch. Trace `StopCause` → `RunFinished.failure` → `GoalFinished.exhausted` → `run_stop` → `run_feedback`, comparing the pinned effective settings with all charged reservations and active-time settlement. Observe exact output, closed-store Events and reduced views.

Use paired cases: (a) genuinely spent finite calls/tokens/active-time/Runs; (b) the same Tool failures with enough remaining budget, including default-unlimited dimensions; (c) insufficient action/Store headroom below the active-time ceiling; (d) a Provider failure on its last allowed call; (e) unknown token usage under a finite cap. Distinguish exhausted budget, legitimate reservation refusal and premature/incorrect refusal. If arithmetic or cause selection is wrong, fix the demonstrated admission/settlement defect in this same flow before polishing the message; do not relabel it as genuine exhaustion. Stop and revise this plan if reproduction exposes a materially different lifecycle. Without the owner's actual Events, report these as reproductions/classifications of the code path, never diagnosis of that specific historical Run. No private-state access or owner question is required to establish this feature's contract.

### Peer basis

[Codex app-server](https://developers.openai.com/codex/app-server#approvals) exposes typed failed/declined items; [Claude Tool failures](https://code.claude.com/docs/en/hooks#posttoolusefailure) preserve specific Tool errors. [Existing peer budget research](../attached-turn-defaults/README.md#research) records default-unlimited turns and OpenCode's final-summary approach. Those precedents support useful feedback but cannot authorize a call past Arany's [durable Goal budget](../../docs/specs/goals.md#lifecycle-budgets-and-recovery). Choose local feedback under that guarantee; no secondary-harness behavior is needed to override it.

Working rules: [RPI/template](../PLANNING.md), [security pass](../../agents/security.md#security-pass-triggers), [evidence rules](../../agents/testing.md), [documentation checks](../../docs/development.md#verification), [Rust practice](../../agents/skills/rust-clean-code/SKILL.md), [architecture review](../../agents/skills/clean-architecture-review/SKILL.md). Ponytail reuses GoalFinished, the ledger and the existing projection rather than adding an explanation agent or a second report store.

## Chosen direction

All decisions below are **Decided 2026-10-22**.

- **Owner / Goals guarantee:** deterministic local explanation only after exhaustion; zero extra Provider calls. No budget reset, automatic new Goal or retry. Preserve an already committed answer and the existing uncertainty/cancellation precedence.
- **Goals accounting:** show calls and Runs as counts, tokens as known charged count plus an explicit unknown-usage flag, and active time in milliseconds. Show pinned effective limit, used-at-refusal, required minimum and requested amount when known. Distinguish a reservation that cannot fit from observed time actually reaching the limit; identify the Store margin explicitly. Retain canonical simultaneous-exhaustion ordering: tokens, calls, active time, Runs.
- **Journal-first guarantee:** add the required nullable **`GoalFinished.budget_refusal`** field to the current payload, carrying a closed `BudgetRefusal` record. This is the authoritative durable home for the refused amount, including out-of-budget resume and failed next-Run admission that closes an existing Goal. The terminal Run and report consume this same fact; there is no prose-parsing or duplicate event stream.
- **Owner / architecture:** use one shared semantic projection for attached, exec/show and serve/ACP. No dependency, setting, legacy-default conversion or compatibility reader. Apply the ordinary payload-version/migration fixture rule for this stored-shape change; absent historical detail is unknown, never reconstructed from invented reservations.

### Durable refusal record and ordering

`BudgetRefusal` contains `dimension: calls | tokens | active_time | runs`, `cause: exhausted | unknown_usage | insufficient_headroom`, `action: provider | tool | delegation | compaction | run | resume`, nullable `run_id` and `agent_run_id`, `used`, finite `limit`, nullable `requested` and `required_minimum`, `token_usage_unknown`, and `charge_basis: observed | conservative`. Values are bounded unsigned integers in the dimension's units; IDs must belong to the Goal and actor. The record contains no paths, arguments or free text and is capped at 1 KiB serialized. `requested` is the unclamped requested amount; `required_minimum` is the actual admission threshold including Store overhead when applicable. For tokens, if the next call's demand is unknown, both remain null; never pretend a token reservation existed. A calls/Run refusal normally requires one unit. The record is non-null exactly for a newly recorded budget-caused Goal closure; it is null for other closures. Do not classify a delegation-only deadline as Goal exhaustion. Preserve the existing reducer predicate for `GoalFinished.exhausted`: a requested Tool interval or configured timeout refusal while other actions still fit is not automatically an exhausted Goal. Such recoverable action limits keep their existing Tool Limit result; the classification step must distinguish them from terminal Goal budget refusal.

Capture the typed refusal snapshot at the serialized admission boundary, carry it through settlement, then commit it with `GoalFinished` using reserved terminal headroom after the Run's terminal facts when there is a Run. Only after acknowledgment may durable UI/protocol feedback name that reservation as committed. Reopening reads the same record; `used` means **at refusal**, not final settlement after terminal-tail work. The reducer validates effective finite limits, IDs, cause/dimension/arithmetic consistency, and the snapshot against prior committed counters/ceilings and terminal settlement; active-time snapshots may precede the final tail charge and cannot exceed its valid settled total. Unknown tokens and conservative crash charges retain their qualifications. This record does not change or refund the ledger. Update the payload registry, migration fixture, strict decoder/reducer and terminal-headroom sizing together.

**Before commitment:** refusal before any Goal is admitted, or a Store/acknowledgment failure before `GoalFinished`, produces only a bounded local admission/infrastructure diagnostic with available values explicitly labelled uncommitted. It never claims durable exhaustion or invents a receipt. Reopen exposes only the committed prefix (possibly Interrupted) and existing conservative recovery facts; a missing refused amount stays unavailable. A Store failure cannot be repaired by making a Provider call. Thus live durable and reopened feedback agree, while a local diagnostic is clearly separate.

Final feedback identifies the terminal cause plus at most eight recent failed/denied/limited Tool facts, bounded by the existing 8 KiB report and 2 KiB item limits; uncertainty/blocking cause precedes optional details. Name Tool and actor, available exit/limit code, omission count and safe admitted excerpt. No raw hook/client denial reason or permission-rule text enters model context or this summary: use compiled labels, shared escaping and the same cap as [Tool continuation](../failure-feedback/README.md#feedback-contract). G1 applies those compiled labels to its local summaries without depending on C1 implementation. Reports are Arany status, never fabricated assistant `MessageCommitted` text. Existing JSONL remains canonical Events.

Engine owns capture/accounting, the reducer owns history validation, and presentation/transport adapters own formatting. These remain in-process boundaries; no wire type enters budget policy. Linux/macOS policy is identical, with no new native-support claim.

## Contract and scenarios

Update [Goals lifecycle/reports](../../docs/specs/goals.md#lifecycle-budgets-and-recovery) to name the durable refusal snapshot alongside in-memory reservations; [Execution accepted outcomes/payloads](../../docs/specs/execution.md#accepted-outcomes) for cause/field ordering; [Terminal failure feedback](../../docs/specs/terminal.md#conversation-progress-and-history), [Serve](../../docs/specs/serve.md) and [ACP](../../docs/specs/acp.md) for projections. Tool denial continuation and approval-expiry copy are owned by their other plans.

Required observations: the classification matrix above; all four finite dimensions and simultaneous limits; unknown tokens; active reservation minimum versus elapsed exhaustion; a failed Provider at a boundary; preserved existing answer; out-of-budget resume; pre-Goal refusal; lost terminal acknowledgment; poisoned/oversized error output; child/primary cause precedence; zero Provider/effect dispatch on strict replay. Extend existing [Goal](../../crates/arany/tests/engine_run/goals.rs), [ledger](../../crates/arany/src/engine/ledger/tests.rs), [stop projection](../../crates/arany/src/session/stop.rs), [presentation](../../crates/arany-cli/src/presentation/model.rs), [Event](../../crates/arany/src/session/events/tests.rs), [migration](../../crates/arany/src/store/migrate/tests.rs), [serve](../../crates/arany-cli/tests/serve/in_process.rs) and [ACP](../../crates/arany-cli/tests/acp/in_process.rs) corpora; exact output, reopened file-backed Events and reduced views must agree.

## Security and resource model

Stored facts, cost, prompt/output handling trigger the security pass. Protect paid usage, private Tool output and journal integrity. Model/repository/Tool text grants no budget. Keep startup/private-directory admission, request/context/Store bounds, finite action limits and descendant restrictions. The new record has closed labels, a 1 KiB cap and reserved terminal capacity. Report overflow drops optional details with an omission count, never uncertainty or cause. No secrets, hidden paths, raw upstream bodies or filesystem rediscovery; output backpressure cannot hold settlement/cancel. Unknown effects stop continuation. No aggregate-spend exception or model-quality guarantee is introduced.

## Risks and mitigations

- Generic copy conceals premature refusal → classification is G1's first task, before rendering changes.
- In-memory requested amount disappears on reopen → commit the snapshot in the named GoalFinished field; distinguish local pre-commit diagnostics.
- Tail settlement differs from the refusal instant → label used-at-refusal and final usage separately; validate both without double charging.

## Vertical slices

1. **G1 — Every Goal budget stop explains its real limit and prior failures without another model call.** Begin with the classification matrix, then cross admission/ledger → typed refusal snapshot → GoalFinished/reducer/version fixture → shared terminal/exec/show/serve/ACP projection → owning specs and existing evidence. Fix any demonstrated premature-refusal defect before final output assertions; include genuine exhaustion, insufficient headroom, unknown usage, Provider failure, crash/Store refusal and no extra dispatch. One PR, independently usable before approvals or Tool-continuation work.

## Exit criteria

- **Research gate passed:** current generic-copy path, in-memory reservation gap and terminal owners are verified. Historical incident cause is explicitly unknown, not a design premise.
- **Plan gate passed:** durable carrier, lifecycle and local fallback are decided; no owner-only question remains.
- **Implement gate:** G1 begins with reproduction/classification and proves exact visible/durable facts and zero extra calls. Required formatting, Clippy, targeted tests and full CI occur during implementation, not this documentation update.
- Track unavailable native/live observations in existing [NEXT_STEPS](../../NEXT_STEPS.md) owners during implementation; close this plan/index row under the [planning lifecycle](../README.md#lifecycle) on delivery.
