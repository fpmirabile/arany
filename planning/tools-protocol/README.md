# Tool protocol and failure visibility

## Metadata

- Status: `Draft`
- Owner: Fernando Mirabile
- Last updated: 2026-10-19

## Outcome and scope

Arany must be usable as an ordinary coding chat before it can be trusted to work autonomously. The owner could not use it productively on a ChatGPT-plan account. This plan first restores a basic chat loop that works end to end: one answer per turn, no forced Tool on trivial turns, writes that work within the admitted grant, and failures that are honest and visible to both the model and the user. It then adds deterministic slash Skill invocation and peer-style Tool rows.

It changes the agent protocol only: what the model is asked to produce at each step, and how refusals and failures flow back. The execution model (journal-first Events, Guard, typed intents, receipts, strict replay) stays as it is. Out of scope: parallel Tool calls ([next steps item 10](../../NEXT_STEPS.md#pursue-in-order) owns them), compaction and Tool-review encodings (internal typed calls, no reported defect) and Supervisor work.

Every choice follows [priority 4](../../AGENTS.md#product-and-implementation-priorities). The owner settled the decisions as peer parity (Claude Code, Codex CLI, OpenCode, Pi); see [decisions](#decisions). A divergence remains only where one of the [five guarantees](../../AGENTS.md#non-negotiable-guarantees) requires it, and the owning spec names it.

## Reported failures and evidence

Verified on `main` at 633fa826. The read-only analysis is in `harness-team/logs/tools-protocol-review.md` (outside the repository); corrections to it are listed under [Corrections to the analysis](#corrections-to-the-analysis).

| # | Owner report | Cause in code |
|---|---|---|
| F1 | `hola!` ran `Tool · List . · succeeded` before answering. | Every Tool-enabled ChatGPT call sends `tool_choice: "required"` (`provider/openai/subscription.rs:541`); the API route sends `allowed_tools` with `mode: "required"` (`provider/openai/wire.rs:346-351`). Finish is a function (`tool_specs.rs:46-82`), and its description says "report its observed result". `primary.txt` opens with "Use the supplied native Tools to complete the user's current task, then return the Finish outcome". Each user message also carries a `goal` budget block and the Tool catalog (`provider/render.rs:169-174`), because a plain attached turn starts a default Implementation Goal (`arany-cli/src/cli/attached/active.rs:149`, `engine.rs:262-268`). `List .` is the cheapest prelude Tool that satisfies "call a function first". |
| F2 | The answer read "Workspace inspected successfully; no changes were requested or made.", and the real greeting appeared later as a `Resumed: Hola! ¿En qué te gustaría trabajar…?` notice. | Finish has two undocumented strings, `summary` and `result` (`provider.rs:116-124`). The model put the greeting in `summary` and a report in `result`. `MessageCommitted` holds `result` (`run_loop.rs:1010-1013`), which became the answer. The generated Session title is the first Finished Run's `summary` (`session/reducer.rs:795-812`), and both resume paths print `Resumed: {title}` (`cli/attached.rs:1303`, `cli/attached/controls.rs:731`). This was not a second Run: a plain turn is a non-autonomous Goal with `finish_after_run: true` (`engine.rs:204-212`). |
| F3 | The notices `Command locked for this run` and `Run in progress... Ctrl+C cancels` appeared during the greeting. | These lines respond only to input typed during an active Run. A locked slash command gives `Command locked for this run` (`controls.rs:955-957, 1028`); Enter on an empty draft gives `Run in progress...` (`controls.rs:644`). No code path prints them unprompted. The copy does not name the command or what to do instead. |
| F4 | `/ask-matt` + Enter became `$ask-matt`; sending it ran `List .` and greeted. | Enter on a slash-menu Skill inserts `$NAME ` without sending (`terminal/composer.rs:486-491`, [terminal](../../docs/specs/terminal.md#interactive-commands-and-completion)). The composer lists only admitted names (`composer.rs:121-131`), so the Skill was admitted. `$NAME` is only a prompt hint ("load its SKILL.md through the Skill Tool", `primary.txt`), so loading depends on the model; under forced selection it chose `List`. |
| F5 | "write test.txt" failed with "Provider returned an incompatible response"; the log held only `RootPlan InvalidResponse reason=None`. | A decoded, well-formed `arany_write` was refused by `well_formed_tool` (`run_loop.rs:902-957`). The likely triggers are a missing `plan_contract` on a non-prelude Tool; a contract budget with `null` where the default Goal pins a limit (`limit_is_within`, `session/events/goal.rs:106-112`); or `max_provider_calls: 1`, which fails because the current call is already reserved (`reducer/goal.rs:280-283`, then `admits_next_run`, `goal.rs:74-84`). `call_record` sets `failure_reason` only for `CallResult::Failed` (`run_loop.rs:1242, 1309-1316`), and the refused proposal is not journaled (`requested_tools` is 0). The fallback copy blames the Provider (`arany-cli/src/presentation.rs:722`). |
| F6 | On the next turn the agent did not know the Run had failed. | Context compilation skips every non-Finished Run with no Tool or worker history (`session/context.rs:114-119`). |

There are other silent failures of the same kind. Three post-decode checks return `InvalidResponse(None)` (`run_loop.rs:1183, 1190, 1197`). These paths call `fail_primary` and leave no cause on the Run: plan admission refusal after a `ToolRequested` record (`run_loop.rs:959-973, 627-629`), Tool-result context exhaustion (`SizingRefusal::Unavailable`, `step.rs:259, 412, 434`), unverified workers, and an unmet criterion (`run_loop.rs:982-1000`).

### Corrections to the analysis

- The attached Goal creation it cites (`cli/attached/run.rs:1278`) is a test; the production site is `cli/attached/active.rs:149`.
- `call_record` starts at `run_loop.rs:1223`; line 1242 is its `Response(_) => InvalidResponse` arm.
- "Strict replay already handles non-dispatched Denied observations" does not hold for a refused Write. With no recorded contract, the reducer accepts `ToolStarted` only for a prelude call (`session/reducer/tool.rs:384-399`), so a refusal cannot be journaled as a `ToolStarted`/`ToolFinished` pair. The precedent to reuse is the worker-coordination refusal: no intent is journaled, a closed reason is recorded on the call, and the next request carries `last_answer_refused` ([execution](../../docs/specs/execution.md#worker-journal-protocol), `run_loop/children.rs:708-728, 1780-1786`).
- New closed reason variants bump `ProviderCallRecorded`. #138 bumped it when it added the worker reasons, and replay validates the reason-to-disposition pairing (`session/events/records.rs:535-570`).
- Anthropic does not end turns with plain text today. Finish and Delegate are strict JSON text under `output_config.format` (`anthropic/wire.rs:400-420`, [Provider](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding)).
- The analysis missed F2 and F4.

## Decisions

Each row is **decided: peer parity** unless marked otherwise. The guarantee column names what still constrains the behavior; the owning spec records each divergence.

| # | Decision | Peer citation | Guarantee note |
|---|---|---|---|
| D1 | Primaries send `tool_choice: "auto"`; an assistant message with no function call ends the turn and is the answer. | Claude Code: `tool_choice: auto`, a text `end_turn` ends the turn. Codex CLI: `tool_choice: "auto"`, the turn ends when a response has no function call. OpenCode and Pi behave the same. | Journal-first: still exactly one committed outcome per step (answer, Delegate or one Tool). |
| D2 | No Finish function and no `summary`/`result` split. The answer is the text; the Engine derives the bounded `AgentFinished.summary` locally from it. | No peer has a finish Tool or a summary field. | None. |
| D3 | Delegate stays a function. | Claude Code's Task/Agent Tool; OpenCode's `task` Tool. | Durable Goals: a delegation is a team transition and stays journaled. |
| D4 | ChatGPT switches to `tool_choice: "auto"` with its phase-dependent list without waiting for probe P3. P3 shrinks to the constant-list `allowed_tools` cache optimization. | Codex CLI is the reference ChatGPT-backend client and sends `tool_choice: "auto"`. | None. |
| D5 | A refused proposal (invalid arguments, plan, scope, lease or phase) goes back to the model as a fixable error, and the Run continues. | Claude Code: `tool_result` with `is_error`. Codex: function error output. Pi: `isError` results. OpenCode: error part. | Per-effect isolation: nothing dispatches. Journal-first: the refusal is committed on the call before the next step. |
| D6 | Refusals have no separate retry budget beyond the Goal budget, except that three consecutive identical refused proposals end the Run with the closed cause. | OpenCode's doom-loop threshold of three identical calls. Claude Code and Codex rely on turn and step limits. | [Global rule 4](../../AGENTS.md#global-rules) needs a declared bound because the Goal call budget may be unlimited. |
| D7 | Failed, cancelled, interrupted and refused turns stay in the model's transcript. | Claude Code keeps errored `tool_result`s. Codex keeps the call and its error. OpenCode shows error parts. | Honest uncertainty: an Uncertain effect is still rendered as unknown and never replayed. |
| D8 | The plan becomes an optional `arany_plan` Tool, never a field on every mutating Tool. Without a plan, Tools dispatch under the pinned grant, permission mode, hooks and Guard. | Codex `update_plan` and Claude Code TodoWrite are optional and never required per call; Pi has no plan Tool. [Next steps item 1](../../NEXT_STEPS.md#pursue-in-order) already proposes `arany_plan`. | Safe AI cage: a plan only narrows. Unlike the advisory peer plans, a recorded plan still binds scope and the success gate ([Goals](../../docs/specs/goals.md#initial-plan-contract-and-success-receipt)); goals.md names this divergence. |
| D9 | One assistant answer per turn. Earlier text in the same Run is progress commentary. | Codex `commentary`/`final_answer` phases; Claude Code's last assistant text. | None. |
| D10 | The generated Session title is the first objective's preview. | Codex previews threads by the first user message. Claude Code's model-generated title would need an extra Provider call per Session, which conflicts with durable budgeted Goals (every call is charged and journaled). | Durable Goals. |
| D11 | Skills are invoked by slash only. `/name [task]` + Enter sends the turn; the `$NAME` form is removed with no compatibility reader. **Decided: owner + peer parity.** | Claude Code `/skill-name`; Pi `/skill:name`; Codex now also slash (owner's reading; Codex docs still list the `$` mention). | None. |
| D12 | On an explicit invocation the harness attaches the pinned `SKILL.md` deterministically; listed resources stay lazy, and model-chosen Skill loading stays as it is. | Claude Code injects the Skill on `/skill-name`. Pi: "Explicit `/skill:name` invocation loads the instructions directly". | Safe AI cage: Skill text stays bounded, digest-pinned, untrusted data and grants nothing ([Tools](../../docs/specs/tools.md#portable-skills)). |
| D13 | `disable-model-invocation: true` removes the Skill from the model-visible list; explicit invocation still works. | Claude Code and Pi (`disable-model-invocation`); Codex `allow_implicit_invocation: false`. | It only narrows. |
| D14 | Enter on a slash-menu entry invokes it, for both commands and Skills; Tab completes. **Decided: owner + peer parity.** | Claude Code. | None. |
| D15 | Tool rows read `⏺ Operation(arg)` with a `⎿ result` line; successes show no state word; consecutive successful reads collapse. | Claude Code `⏺ Tool(arg)` / `⎿ summary`; Codex groups reads as "Explored". | Per-effect isolation and honest uncertainty: `UNGUARDED`, uncertainty, enforcement-cause, checkpoint and denial-source markers stay mandatory ([Tools](../../docs/specs/tools.md#unguarded-re-run-after-a-guard-failure)). |
| D16 | A plain turn omits the `goal` budget block unless the Goal is autonomous or has a recorded plan. | Peers send no budget data on ordinary turns. | Durable Goals: the budget is still enforced by the Engine, not by the prompt. |

## Contract and scenarios

Owning specs: [execution](../../docs/specs/execution.md) (accepted outcomes, Events), [Provider](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding), [ChatGPT](../../docs/specs/chatgpt.md#catalog-checks-and-runs), [Session](../../docs/specs/session.md#context-and-compaction), [Goals](../../docs/specs/goals.md#initial-plan-contract-and-success-receipt), [Tools](../../docs/specs/tools.md), [terminal](../../docs/specs/terminal.md#interactive-commands-and-completion) and [diagnostics](../../docs/specs/diagnostics.md). Each slice edits only the sentences it changes. Each fact has one home, and other specs link to it.

Applies to every slice: a change to the decoder, a prompt, request rendering or validation bumps, in the same PR, the native check (`provider/native_check.rs:14`), custom check (`provider/custom/check.rs:14`), ChatGPT model-check and ChatGPT account-admission versions (`arany-cli/src/accounts/chatgpt/account.rs:24-25`), as [Provider](../../docs/specs/provider.md#prompt-caching) and [ChatGPT](../../docs/specs/chatgpt.md#catalog-checks-and-runs) require. Payload bumps follow "one current payload version" with no compatibility reader ([execution](../../docs/specs/execution.md#admission-persistence-and-recovery)), and each one updates `session/events.rs::payload_version`, its table row and the `events/tests.rs` assertions.

## Execution

Slices 1-5 restore the basic chat loop and land first, in order. Slices 6 and 7 follow. Each slice is one PR; stacked PRs are fine ([Git](../../agents/git.md#pull-requests)). All slices apply [Ponytail](../../.agents/skills/ponytail/SKILL.md), [rust-clean-code](../../agents/skills/rust-clean-code/SKILL.md) and [clean-architecture-review](../../agents/skills/clean-architecture-review/SKILL.md).

Order versus the suggested one: the plan-contract change (slice 4) lands before the protocol switch (slice 5). The write failure (F5) blocks real work, and slice 5 alone would not fix it. Removing `plan_contract` from every Tool schema also shrinks the function list that slice 5 then reworks, so slice 5 is a smaller diff. Both slices bump the same check versions.

### Slice 1: honest, journaled failure causes

- **Scope.**
  - `well_formed_tool`, `primary_lease_allows`, the Finish and Delegate checks in `drive_primary_inner` (`run_loop.rs:496-529`) and `well_formed_delegation` return a closed cause instead of `bool`. Causes: `invalid_arguments`, `phase`, `plan_missing`, `plan_invalid`, `plan_budget_outside_goal`, `plan_budget_spent`, `plan_scope_excludes_call`, `plan_outside_grant`, `argument_bytes`, `lease_overlap`, `not_offered`, `finish_invalid`, `delegate_invalid`.
  - `call_record` records them as the new reason `proposal_refused` plus a required nullable `refusal {operation, cause}`, where `operation` is the compiled Tool name, `finish` or `delegate`. No argument values are recorded.
  - The three post-decode `InvalidResponse(None)` sites use `ResponseContract`.
  - `RunFinished` gains a required nullable closed `failure` cause, present exactly with `Failed`: `provider_call`, `proposal_refused`, `plan_admission`, `tool_context_full`, `workers_unverified`, `criterion_unmet`, `refusal_limit`.
  - `diagnostics::event_failure` logs the refusal and the Run cause.
  - `presentation.rs:657-731` replaces the "incompatible response" fallback with copy per cause that never blames the Provider for an Engine refusal.
  - Model behavior is unchanged: a refusal still fails the Run.
- **Payloads.** `ProviderCallRecorded` 9 → 10; `RunFinished` 8 → 9. Replay pairs `proposal_refused` only with `InvalidResponse` and a refusal, and requires `failure` exactly on `Failed`.
- **Guarantees.**
  - Journal-first: the cause is on the same committed Event, and replay validates it.
  - Honest uncertainty is unaffected.
- **Specs.**
  - execution.md: accepted outcomes (closed refusal list), the payload table and `RunFinished`.
  - diagnostics.md: refusal and Run-cause records.
  - terminal.md: Run-failed copy.
- **Tests.**
  - Regression (F5): `crates/arany/tests/engine_run.rs` cases with a scripted Provider that returns `arany_write` once with `plan_contract: null` and once with `max_provider_calls: 1`. Each asserts the journaled cause and that the copy does not name the Provider.
  - `session/events/tests.rs`: the version bump and pairing rejections.
  - `session/reducer/tests.rs`: `failure` presence.
  - `diagnostics.rs` tests: no argument bytes in the record.
  - `presentation.rs` tests: copy for each cause.
- **OS impact.** None.

### Slice 2: refusals the model can fix

- **Scope.**
  - A slice-1 refusal no longer ends the Run. The Engine commits the refused call record and continues the loop.
  - The next request carries a `last_answer_refused` note with the operation, the cause and fixed remediation text for that cause (for example "send the call without `plan_contract`" or "the budget must not exceed the goal budget"). This reuses the worker-coordination channel (`children.rs:1780-1786`) and echoes no model-authored values.
  - A crash before that request keeps the refusal, because it was committed first.
  - Three consecutive refusals with an identical `(operation, cause)` end the Run with `refusal_limit` (D6).
  - Each retry is a counted Provider call under the Goal budget.
  - Explicit `team` planning that returns an answer instead of a Delegate is refused through the same channel.
- **Payloads.** None. Request rendering changes, so the check versions are bumped.
- **Guarantees.**
  - Per-effect isolation: no intent, Guard dispatch or receipt for a refused proposal.
  - Journal-first: the refusal commits before the next reservation.
  - Durable Goals: retries spend the pinned budget, and pause and cancel boundaries are unchanged.
  - Safe AI cage: the note is closed, Engine-authored text.
- **Specs.**
  - execution.md "Accepted outcomes": replace "policy-incompatible outcomes reject without repair" for the primary with this rule and its cap.
  - tools.md "Limits and failure behavior": the cap.
- **Tests.**
  - Engine corpus: a refused Write, then a valid Write that dispatches through the Guard; three identical refusals end with `refusal_limit`; crash after the refusal, then resume shows the refusal.
  - Guard journey (`crates/arany-cli/tests/guard_runs.rs`): no process or mount for a refused call.
  - `render/tests.rs`: the note.
- **OS impact.** None.

### Slice 3: failed turns stay in the transcript

- **Scope.**
  - `CompiledContext::compile` (`session/context.rs:114-135`) also selects non-Finished Runs without Tools.
  - Such a Run renders as a `previous_run (historical, untrusted)` turn: the objective plus a typed fact built only from committed Events. Examples: "Run failed before any Tool: Write refused, plan budget outside goal", "cancelled by the user", "interrupted", "stopped by hook".
  - The wording matches the unanswered statuses in compaction (`session/compaction.rs`).
  - `MAX_HISTORY_RUNS` and the byte accounting are unchanged.
- **Payloads.** None. The compaction compiler version is unchanged because compaction input is unchanged. The render change bumps the check versions.
- **Guarantees.**
  - Honest uncertainty: an Uncertain observation still renders as `uncertain; never replay`; a refusal renders as nothing dispatched.
  - Safe AI cage: a labelled, untrusted block.
- **Specs.** session.md "Context and compaction": failed, cancelled and interrupted Runs without Tools enter history as a bounded typed fact.
- **Tests.**
  - Regression (F6): an Engine test where a failed Write Run is followed by "why couldn't you write it?" and the request contains the objective and cause.
  - `session/tests.rs`: the budget still holds with failure-heavy history.
  - `render/tests.rs`: a snapshot of the block.
- **OS impact.** None.

### Slice 4: optional `arany_plan` Tool

- **Scope.**
  - Remove `plan_contract` from every Tool schema (`provider/catalog.rs::plan_contract_schema` consumers) and from `ToolRequest`.
  - Add one optional `arany_plan` function that takes the existing `GoalPlanContract`. A call to it is one step: the Engine validates it (refusals as in slice 2), appends the existing `GoalPlanContractRecorded`, and returns a result naming the recorded scope.
  - Without a recorded plan, any call the phase and pinned grant allow dispatches under the permission mode, hooks, Supervisor and Guard.
  - With a plan, the existing scope check, binding and success gate apply unchanged.
  - The Research and Plan Goal phases keep their prelude-only rule.
  - The reducer's `ToolStarted` rule (`reducer/tool.rs:384-399`) drops the "awaiting contract → prelude only" branch.
  - The `primary.txt` plan paragraph shrinks to when a plan helps: long autonomous Goals and exact success criteria.
- **Payloads.** `ProviderCallRecorded` 10 → 11 adds the disposition `plan_recorded`. `GoalPlanContractRecorded` is unchanged.
- **Guarantees.**
  - Per-effect isolation and the safe AI cage: authority still comes from the pinned grant and Guard; the plan never granted authority and still only narrows.
  - Durable Goals: budgets are pinned at `GoalStarted`. An autonomous Goal without a plan already completes on its first Finish ([Goals](../../docs/specs/goals.md#autonomous-goals)).
- **Specs.**
  - goals.md "Initial plan contract": the plan is optional; name the D8 divergence there.
  - provider.md: drop the "required-but-nullable `plan_contract`" bullet.
  - execution.md: the prelude sentence.
  - tools.md line 11.
  - Remove next steps item 1's follow-up.
- **Tests.**
  - Regression (F5): an attached-shape Engine test where "write test.txt" dispatches a Write with no plan, under Ask approval, and the file exists.
  - An `arany_plan` call that narrows scope, after which an out-of-scope Write is refused as a result.
  - The success gate is unchanged when a plan exists.
  - Reducer corpus for the new disposition and relaxed rule.
  - Wire corpora (`openai/wire/tests.rs`, `openai/subscription/tests.rs`, `anthropic/wire/tests.rs`): no `plan_contract` in Tool schemas.
- **OS impact.** None.

### Slice 5: text answers with `tool_choice: auto`

- **Scope.**
  - OpenAI API (`wire.rs:346-351`): `allowed_tools` with `mode: "auto"`.
  - ChatGPT (`subscription.rs:541`): `tool_choice: "auto"`. Both remove `arany_finish` and keep `arany_delegate`, `arany_plan` and `parallel_tool_calls: false`.
  - Anthropic (`anthropic/wire.rs:400-420`): drop `output_config.format` for Runs. Delegate becomes a Tool, and an `end_turn` text is the answer.
  - Decoding: a response with exactly one function call is that outcome, and earlier message text is commentary. A response with only final message text is the answer. Two or more calls and incomplete calls still reject, through slice 2.
  - An exhausted researcher uses `tool_choice: "none"` and answers in text. Read-only and child routes answer in text too.
  - The Engine derives `AgentFinished.summary` (first nonempty line, truncated on a character boundary to `MAX_FINISH_SUMMARY_BYTES`). `valid_finish` (`run_loop.rs:1331`) checks only the answer.
  - The generated title uses the first objective (D10, `reducer.rs:795-812`).
  - `primary.txt` follows the Codex and Claude Code shape: answer directly when no Tool is needed; use Tools when the task needs files, commands or Skills; one Tool per step; keep going until the task is done; the final answer is plain text.
  - `render.rs:169-174` applies D16.
  - Compaction and Tool review keep their structured encodings.
- **Payloads.** None: `MessageCommitted` and `AgentFinished` are unchanged. The check and account-admission versions are bumped. Probe P3 is rewritten in NEXT_STEPS (D4).
- **Guarantees.**
  - Journal-first: one committed outcome per step; the answer is still `MessageCommitted` after validation.
  - Honest uncertainty and the cage are untouched; the catalog still bounds calls.
- **Specs.**
  - provider.md "Structured outcomes and Tool encoding" and "Prompt caching": the function list and text answers. Name the remaining divergences: parallel calls off and the plan binding.
  - execution.md "Accepted outcomes": remove "final text rejects".
  - chatgpt.md: the request shape.
  - tools.md line 11.
  - session.md: the title rule.
- **Tests.**
  - Regression (F1, F2): an Engine test where `hola!` produces one answer equal to the model text, no Tool and one Provider call.
  - `resume` shows the objective title.
  - Wire corpora: text-only answer; commentary plus call; two calls reject; exhausted researcher.
  - Native and ChatGPT check fixtures.
  - Replay corpus: `MessageCommitted` unchanged.
- **OS impact.** None.

### Slice 6: slash Skill invocation

- **Scope.**
  - Composer (`composer.rs:121-131, 478-495`): Skills complete as `/name`, and Enter on any menu entry submits (D14). Remove the `$NAME` form, its completion and the `primary.txt` sentence.
  - A submitted `/name [task]` that matches an admitted Skill (compiled commands keep precedence) becomes a turn with objective `task` and the invoked Skill.
  - Before the first Provider call, the Engine runs one Engine-originated `Skill { name, resource: null }` read through the existing Skill path: the Linux read-only mount, the macOS checked handle and the pinned digest. The reducer gets an origin rule like `ParseAgentDefinition` (`reducer/tool.rs:405-421`).
  - The committed observation renders as a labelled `invoked_skill (untrusted)` block in that turn's message.
  - If the read fails, the Run fails with a closed cause before any Provider call. An unadmitted name gives the existing unknown-command notice.
  - Apply D13.
- **Payloads.** `MessageAccepted` 6 → 7 adds a required nullable `skill`. `ToolStarted` is unchanged. The check versions are bumped.
- **Guarantees.**
  - Isolation: the same admitted read capability and bounds; no new authority.
  - Safe AI cage: Skill text is data, digest-pinned and bounded by the Tool result ceiling.
  - Journal-first: the read is a committed intent and observation before the call.
- **Specs.**
  - terminal.md:125: slash-only Skills and Enter invokes.
  - tools.md "Portable Skills": explicit attachment, D13, and the `$NAME` sentence removed.
  - execution.md: the payload row.
- **Tests.**
  - Regression (F4): `crates/arany-cli/tests/session_run/slash_completion.rs` covers `/ask-matt` + Enter submitting, and `/ask-matt do X` producing objective `do X` with the Skill.
  - Engine test: the first request contains the pinned SKILL.md block, and a digest mismatch fails before egress.
  - Reducer origin rule.
  - Guard Skill journeys on Linux and macOS.
- **OS impact.** Uses the existing Linux mount and macOS file worker. macOS needs native evidence before claiming support.

### Slice 7: peer-style Tool rows and active-Run notices

- **Scope.**
  - `tool_summary` (`presentation.rs:100-200`) applies D15. It leads with `⏺ Operation(arg)`, adds a `⎿` line with a bounded result summary (entries, lines or bytes; `applied`; the failure text), drops `Tool ·` and `succeeded`, and collapses consecutive successful reads.
  - Active-Run notices name the command and the alternative, for example "/model is unavailable while a Run is active; Ctrl+C cancels".
  - Linear and screen-reader output uses the same words without glyph dependence.
  - Load the `arany-terminal-design` skill.
- **Payloads.** None.
- **Guarantees.** None changed. The mandatory markers stay (D15).
- **Specs.** terminal.md "Conversation, progress and history": the row format, grouping and accessible equivalents.
- **Tests.**
  - `presentation.rs` and `presentation/model.rs` snapshots at 80 and 120 columns.
  - Linear-mode lines.
  - PTY journey in `session_run/active_terminal.rs` for grouping and the notice (F3).
- **OS impact.** Terminal only; covered on both targets by existing PTY fixtures.

## Security and resource model

[Security-pass triggers](../../agents/security.md#security-pass-triggers):

- Slice 1: diagnostics privacy.
- Slice 2: model-facing refusal text.
- Slice 6: Skill content sent to the Provider.

Bounds:

- Refusal notes are closed text: at most one per request, and at most three identical refusals in a row.
- Derived summaries are capped at 2 KiB.
- The attached Skill is capped at `MAX_TOOL_RESULT_BYTES` (64 KiB) and runs the ordinary credential-reflection check.
- History facts stay inside the existing per-Run and byte budgets.

Nothing echoes model-authored argument values into logs.

## Risks and mitigations

- **A text answer ends a task early** ("I'll now edit…"). Mitigations: the Codex-style persistence wording in `primary.txt`, Stop hooks, and the Goal criterion when a plan exists.
- **Retries spend plan quota.** Mitigations: D6, and every retry is visible as a refusal row.
- **Version bumps invalidate stored check evidence.** The owner reruns `provider check` (plan-consuming). Admission needs no check record ([ChatGPT](../../docs/specs/chatgpt.md#catalog-checks-and-runs)).
- **Prompt cache misses.** The function list changes once per slice, then is stable.
- **Rollback.** Revert the slice PR. Journals written with a bumped payload version reject on the reverted binary without changing their bytes; recover with the [explicit history reset](../../docs/specs/terminal.md#setup-and-account-choices). This is accepted pre-beta practice.

## Open questions

None. No decision conflicts with a guarantee: every divergence above is a narrowing or a bound that a guarantee or global rule requires, and it is named in its owning spec.

## Exit criteria

- All seven slices are merged with green CI. Each slice's regression test reproduces its reported failure (F1-F6).
- The specs listed per slice describe the current behavior. NEXT_STEPS records the rewritten P3 and any unverified macOS live checks.
- The owner passes the live check below on a ChatGPT-plan account, in a new Session in a trusted project, in attached chat. Results go to NEXT_STEPS until stable pre-beta.
  1. `hola!` shows exactly one `Arany:` answer, which is the greeting, with no Tool row, no notice and one Provider call in `arany show`.
  2. "write test.txt containing hello" shows one `⏺ Write(test.txt)` row with `⎿ applied` and one answer, and `test.txt` contains `hello`.
  3. "write ../outside.txt" shows a denial row. Then "why didn't that work?" gets an explanation of the denial.
  4. Submit "write a.txt", press Ctrl+C before any Tool row, then ask "what happened to my last request?". The agent says it was cancelled and nothing was written.
  5. `/ask-matt` + Enter starts a Run immediately with a `⏺ Skill(ask-matt)` row, and the answer follows the Skill.
  6. `/ask-matt which skill fits a refactor plan?` + Enter: the answer applies the Skill to that question.
  7. `/resume` after check 1 shows `Resumed: hola!`.
  8. `/model` + Enter during a Run shows the named-command notice.
  9. For a forced refusal (from the slice 2 test fixture), `development.log` holds the closed refusal cause, and `arany show` lists it on the call.
- Closeout under the [planning lifecycle](../README.md#lifecycle): delete this plan when the last slice lands.
