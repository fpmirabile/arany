# Engine loop: decide, then apply

## Metadata

- Status: `In progress`
- Owner: Fernando Mirabile
- Last updated: 2026-10-19

## Outcome and scope

The Run loop gets one owner for every decision it makes: why a step stopped, what each AgentRun and the Run journal at the end, whether an effect dispatches, how a child or worker ends, and whether a failed Provider attempt retries. Today these decisions are computed inline at effectful sites from ambient state (the ledger view, the cancellation token and mutable flags). They travel through lossy channels (`bool`, `Option`, `Err` used for control flow, prose strings). The same precedence is copied with different orders at ten sites. Every recent Engine review found a misclassified stop cause there.

The pattern is a functional core with an imperative shell ("decide, then apply"). Each decision is a pure function over facts the shell already fetched or awaited, and it returns a closed enum. The shell keeps today's exact sequence of awaits, appends and Guard dispatches. The session reducer stays the only evolve, so the Engine becomes the decide half of a decide/evolve pair whose evolve half is already replay-validated. [Library practice](../../agents/arany.md#engine-loop-decisions) holds the working rule; this plan migrates the existing loop to it.

This serves the harness responsibility for agent execution and durable Sessions ([priority 1](../../AGENTS.md#product-and-implementation-priorities)). Every slice must keep the [five guarantees](../../AGENTS.md#non-negotiable-guarantees). Journal-first ordering, strict replay and honest uncertainty are restated per slice.

In scope:

- The Run loop: `crates/arany/src/engine/run_loop.rs`, `run_loop/step.rs`, `run_loop/children.rs`, `run_loop/worker.rs` and the attempt loop in `engine/routing.rs`.
- The closed stop reason returned to the `exec`, `serve` and `acp` adapters.
- Bounded, journaled Provider retries.

Out of scope:

- The Guard, `GoalLedger`, the reducer's validation rules, HookFold, Provider adapters beyond retry classification, parallel Tool calls ([next steps item 10](../../NEXT_STEPS.md#pursue-in-order)) and the daily-use items of the viability review that are not architecture (snapshot limits, network, Git writes, Tool ergonomics).

OS impact: none. The loop is platform-neutral Rust. The Guard journeys that pin ordering already run on Linux and macOS CI, and no slice changes a native adapter.

## Current facts and evidence

Verified on `main` at 6b2a3d69. Sources: the read-only proposal `harness-team/logs/engine-refactor-proposal.html` (outside the repository), its review logs, and the code below. [Corrections to the proposal](#corrections-to-the-proposal) lists where they differ.

| # | Problem | Evidence on main |
|---|---|---|
| P1 | The step result is a `bool`. | `execute_step` returns `Result<bool, EngineError>` (`run_loop/step.rs:227-361`), with ten `return Ok(false)` sites for nine reasons (`:257, :259, :271, :274, :285, :302, :311, :331, :332, :356`) and a final boolean expression (`:359-360`). `SizingRefusal::Unavailable` merges active time and context exhaustion (`:76-79`). The four loop files hold 37 `Ok(false)` returns (run_loop.rs 7, children.rs 6, worker.rs 5, step.rs 19). |
| P2 | Stop reasons travel as errors. | A hook active-time stop becomes `Err(GoalBudgetExhausted)` (`step.rs:333`). Store admission refusal becomes `Err(Tool(Limit))` for workers only (`step.rs:845-852`). The four loop files match `GoalBudgetExhausted` 33 times and `ToolError::Limit` 10 times. Callers then re-derive the cause: `is_cancelled()` has 31 sites and `active_time_exhausted` 28. |
| P3 | The precedence is copied, and the copies disagree. | Primary: `settle_stopped_primary` (`run_loop.rs:198-282`: Uncertain, then not-settleable, then cancelled, then hook, then budget), `fail_primary` (`:1049-1072`: cancelled, then hook, with no uncertainty check), `finish_primary` (`:975-1022`), `settle_definition_stop` (`:1024-1047`) and the `Exit` ladder (`:537-545`). Children: `drive_children_inner` (`children.rs:1078-1096`: budget before cancelled), `settle_delegation_inner` (`:316-334`: active time journaled as Budget), `unstarted_abort` (`:1513-1527`) and `hook_abort`, which maps an uncertain hook to `Failed` (`:1369-1382`). Workers: two ladders in `complete_worker_inner` (`worker.rs:855-913`). |
| P4 | Flag state drives the child loop. | `ChildQueue` carries `budget_stopped`, `refusal: Option<&'static str>`, `deferred_at` and `capacity_error` (`children.rs:60-74`). `AbortReason` is a second, incomplete vocabulary with no Uncertain variant (`:81-87`). `drive_children_inner` is 513 lines under `#[expect(clippy::too_many_lines)]` (`:595-1111`). |
| P5 | Decisions are interleaved with effects. | `before_primary_dispatch` (`step.rs:1151-1299`) computes the disposition with three mutable bools and encodes it as six prose strings (`:1244-1283`). `before_dispatch` reserves mid-decision (`:996-1149`). The wave primary re-implements the primary validity check (`children.rs:707-711` against `run_loop.rs:496-529`). The fallback attempt loop exists twice (`routing.rs:113-174` and `children.rs:645-703`), and its decision is a `bool` (`routing.rs:365-394`). |
| P6 | The edges classify stops from prose or re-derived facts. | `RunResult.stopped` is `Option<EngineError>` (`engine.rs:311-315`, set at `:1094`). `exec` prints its Display text (`arany-cli/src/cli/exec.rs:323-326`). `serve` forwards `reason.to_string()` as `goal.stopped` (`cli/serve/server.rs:177, 720, 773`) and re-derives `turn.stopReason` from Goal facts (`cli/serve/items.rs:141-150`). `acp` maps that two-variant reason plus Run status (`cli/acp/translate.rs:1300-1330`, `protocol/methods.rs:563-566`). |
| P7 | No retry on a transient Provider failure. | Zero same-route retries ([Provider](../../docs/specs/provider.md#usage-provenance-and-reliability), [execution](../../docs/specs/execution.md#accepted-outcomes), [system overview](../../docs/architecture/system-overview.md#5-session-run-and-collaboration-lifecycle)). One 429, 5xx or cut stream fails the call. `ProviderError` already distinguishes `RemoteHttp(u16)`, `HttpServerUnavailable` and `PartialTransport` (`provider.rs:841-867`). |

Code that already follows the pattern, and stays: `permission()` (`step.rs:918`) and the `Permission` narrowings are pure, HookFold is a pure fold, `call_record` (`run_loop.rs:1223`) is a pure projection, and `Verdict` (`step.rs:81-88`) and `definition_stop` (`run_loop.rs:1341-1360`) are closed decision values. The reducer refuses `Finished` with unsettled hooks (`session/reducer/agent.rs:683-690`) and validates reason-to-disposition pairing (`session/events/records.rs:535-570`). That strictness turned each misclassification into a visible replay error instead of a silent wrong journal; no slice relaxes it.

In-flight vocabulary from the [Tool protocol plan](../tools-protocol/README.md): slice 1 (branch `feat/honest-failure-causes`) adds `RunFailure`, `ProposalRefusalCause`, `SizingRefusal::{ActiveTime, ContextFull}` and a transitional `tool_stop_cause()`. Slice 3 landed as #183. Slices 2 and 4 are on `feat/recoverable-refusals` and `feat/optional-plan-tool`.

### Corrections to the proposal

- **Editors already run SubagentStop.** `children.rs:954` calls `subagent_stop` for editors. [Tools](../../docs/specs/tools.md#hook-execution) and [execution](../../docs/specs/execution.md#worker-journal-protocol) specify it, and the `followups.md` note saying otherwise is stale. Decision D7 below therefore routes the existing behavior through `decide_child_call` and fixes only its refused-reprompt cause.
- **The relabel is mostly a restoration.** [Execution](../../docs/specs/execution.md#worker-journal-protocol) already requires InvalidOutcome, TurnLimit and `budget` for a refused reprompt. R3b restores specified behavior, and its spec edit is limited to causes the spec does not yet name.
- **Uncertainty journals no terminal Event today.** `settle_stopped_primary` returns `Err(ToolError::Uncertain)` before any append (`run_loop.rs:265-267`). The reducer then projects the open Run as interrupted on reopen. The proposal's `Terminal { agent, run, failure, uncertain: bool }` would let a value say both "journal Failed" and "unknown". The target design makes that unrepresentable (`Settlement::Unknown`).
- **Counts.** The four loop files hold 33 `GoalBudgetExhausted` matches, not 35; the six-file total including `step/hooks.rs` and `tool_compaction.rs` is 39. The other counts are exact.
- Line numbers in the proposal match 6b2a3d69 within two lines, except `engine.rs:1092`, which is now `:1094`.
- The review logs the proposal cites (`followups.md`, `flaky-tests.md`, `*.review.md`) live outside the repository; this plan relies on them only where the code above confirms the defect.

## Decisions

Peer parity is the default for behavior ([priority 4](../../AGENTS.md#product-and-implementation-priorities)). For the loop's shape the owner chose the best design of each harness, improved under Arany's guarantees, rather than one peer. [Peer facts](#peer-facts) holds the verified citations.

| # | Decision | Source | Guarantee note |
|---|---|---|---|
| D1 | Decide, then apply: pure decision functions over closed enums, consumed by one shell function each, as the decide half over the existing reducer evolve. No trait, typestate machine or effect interpreter. | Proposal §8.3. Peer shapes: the Agents SDK `NextStep` union, OpenCode's tagged step `Outcome`, Goose's `OperationResult`/`RunStatus` and Codex's `TurnAbortReason`. Goose and LangGraph go further and interpret effect lists or a graph; that is rejected because Arany's effects are long-lived, nested and concurrent (Guard dispatch, approval waits, streaming previews, worker handover). Re-proving journal-first ordering inside an interpreter costs more than keeping the explicit shell. | Journal-first: the decision is known before its Event is built. Strict replay: the reducer stays the sole evolve. |
| D2 | One merge order, highest first: Uncertain > Cancelled > HookStop > Paused > ActiveTime > Budget > Capacity > ContextFull > DelegationDeadline > Refused > ToolStopped > InvalidCommandResult. **Decided: owner.** | The peers agree on user abort, then hook stop, then limits, then Tool failure: Codex emits one `TurnAborted { reason }` on interrupt, Claude Code's `continue: false` wins over event decisions, and the Agents SDK and Goose check turn limits at the loop head. | Honest uncertainty: no peer has an unknown outcome. Uncertain ranks first so it is always reported unknown and never retried. |
| D3 | An Uncertain stop journals no terminal Event and returns `Err(ToolError::Uncertain)`, as `settle_stopped_primary` does today, now on every exit path. | AGENTS.md honest uncertainty. | Closes the `fail_primary` gap, where a cancelled-and-uncertain stop journals Cancelled. |
| D4 | `EngineError` is infrastructure only. A stop toward the caller is a closed `RunStop` (D6), not `Some(EngineError)`. | rust-clean-code rule 4; the Claude Code SDK result `subtype` and `terminal_reason`. | Durable Goals: exhaustion stays a typed Goal fact, never a parsed message. |
| D5 | Worker relabels (ToolFailed → TurnLimit, InvalidOutcome, StoppedByHook or Cancelled) land in their own revertible PR, R3b, after the byte-identical structural PR R3a. **Decided: owner.** | Codex and Claude Code ship behavior-visible changes as small PRs; [Git](../../agents/git.md#pull-requests) allows stacked PRs. | Strict replay: R3b is the only slice that changes journaled bytes, and it reverts alone. |
| D6 | A closed `RunStop` goes to the `exec`, `serve` and `acp` edges. It is projected from committed Goal and Run facts by one pure function, so live and historical turns agree. Each adapter maps it to its own vocabulary. **Decided: owner.** | ACP's closed `StopReason`; Claude Code's closed `terminal_reason`; Codex's `TurnAbortReason`. | Strict replay: projected from committed facts, never from the live token. |
| D7 | Editors run SubagentStop exactly as researchers do (already the case on main), through `decide_child_call`. A reprompt refused for budget or deadline ends with cause `budget`, and one refused only by turns ends with `turn_limit`, as [execution](../../docs/specs/execution.md#worker-journal-protocol) specifies. **Decided: owner.** | Claude Code fires SubagentStop for every subagent, matched by agent type; `decision: "block"` keeps it running with the reason as its next instruction. | Safe AI cage: a hook only narrows; the private Finish gate still runs. Arany keeps its eight-per-actor continuation cap, which does not reset on Tool calls ([Tools](../../docs/specs/tools.md#hook-execution)); Claude Code resets its cap on a Tool call. |
| D8 | Bounded Provider retries. A 429, a 5xx before body admission or a stream cut before a decoded outcome is retried on the same route. At most three retries per semantic call, with exponential backoff from 2 s capped at 10 s per gap, honoring a server `Retry-After`. Every wait stays inside the remaining action deadline and Goal active time. Each retry commits a fact before re-dispatch and takes a fresh Provider-call reservation charged to the Goal. A configured fallback chain applies only after same-route retries are exhausted. No hidden retries. **Decided: owner.** | Pi and Goose default to three retries, Codex to four request and five stream retries, and Claude Code and OpenCode to ten. OpenCode retries only before output starts; Codex shows `Reconnecting... n/max`. Three is the lowest peer default and keeps the charged cost small. | Durable Goals: every attempt is journaled and charged, and a finite token budget that unknown usage would violate refuses the retry (the existing fallback rule). Honest uncertainty: a Provider call is not a Workspace effect; nothing was decoded or committed, so no effect is repeated. |
| D9 | Ownership: R1, R4 and R6 are mechanical and delegable; R2, R3a, R3b, R5, R7 and R8 have one author each. The precedence or mapping table goes in the PR body and is reviewed against the table tests. **Decided: owner for R1-R4.** | Proposal §7. | None. |
| D10 | Order: tools-protocol slices 1-5 first, then R1 → R2 → R3a → R3b → R4 → R5 → R7 → R6. Tools-protocol slices 6 and 7 interleave after slice 5. R8 interleaves after tools-protocol slice 5 and before R4 (both touch the duplicated attempt loop). **Decided: owner (proposal decision 3), plus R8 placement.** | Proposal §7 decision 3. | Every peer starts from the chat basics that tools-protocol restores; R5 is then written once against the final `plan_allows` rule. |

## Peer facts

Checked on 2026-10-19 against primary sources: repositories pinned to the commit read, and official documentation. Status is **verified**, **corrected** (the proposal said otherwise) or **unverified** (no primary source available).

| Peer | Fact | Status and source |
|---|---|---|
| Codex CLI | `TurnAbortReason` is closed: `Interrupted`, `Replaced`, `ReviewEnded`, `BudgetLimited`. | Corrected (the proposal omitted `BudgetLimited`). [protocol.rs:4394](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/protocol/src/protocol.rs#L4394) |
| Codex CLI | The completion event is `EventMsg::TurnComplete`, serialized as `task_complete`; `Op::Interrupt` aborts the current task. | Corrected (named `TaskComplete` in the proposal). [protocol.rs:1528](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/protocol/src/protocol.rs#L1528) |
| Codex CLI | `ReviewDecision` is `Approved`, `ApprovedExecpolicyAmendment`, `ApprovedForSession`, `ApprovedMcpPolicyAmendment`, `NetworkPolicyAmendment`, `Denied { rejection }`, `TimedOut`, `Abort`. `AskForApproval` is `UnlessTrusted`, `OnRequest`, `Granular`, `Never`. | Corrected (wider than the proposal's four variants). [protocol.rs:4269](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/protocol/src/protocol.rs#L4269), [:1097](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/protocol/src/protocol.rs#L1097) |
| Codex CLI | `run_turn` loops while the sampled response needs a follow-up (a function call or pending input). Requests send `tool_choice: "auto"`. | Verified. [session/turn.rs:584-617](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/core/src/session/turn.rs#L584-L617) |
| Codex CLI | Retries: `request_max_retries` 4 and `stream_max_retries` 5 by default (user cap 100), on 429, 5xx and transport errors, shown as `Reconnecting... n/max`. | Verified. [model-provider-info/src/lib.rs:67-75](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/model-provider-info/src/lib.rs#L67-L75), [codex-client/src/retry.rs:16-30](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/codex-client/src/retry.rs#L16-L30), [core/src/responses_retry.rs:155](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/core/src/responses_retry.rs#L155) |
| Codex CLI | An interrupted Tool returns `aborted by user` output to the model's history. | Verified. [core/src/tools/parallel.rs:378-383](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/core/src/tools/parallel.rs#L378-L383) |
| Codex CLI | Built-in subagents: the `multi_agent` feature is stable and enabled by default (`spawn_agent`). Tool calls can run in parallel. | Corrected (the proposal said Codex has no subagents and implied serial calls). [features/src/lib.rs:1494-1497](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/features/src/lib.rs#L1494-L1497), [core/src/tools/parallel.rs](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/core/src/tools/parallel.rs) |
| Codex CLI | After a sandbox failure it can ask "command failed; retry without sandbox?", but not under `Never` or `OnRequest`. | Verified. [core/src/tools/orchestrator.rs:439, :644](https://github.com/openai/codex/blob/c3bb20602d77b0c8e62f84e9956e8086f439dfeb/codex-rs/core/src/tools/orchestrator.rs#L439) |
| Claude Code | SDK result `subtype` is `success`, `error_max_turns`, `error_during_execution`, `error_max_budget_usd` or `error_max_structured_output_retries`. A closed `terminal_reason` adds 19 values, among them `completed`, `max_turns`, `aborted_tools`, `hook_stopped`, `stop_hook_prevented`, `prompt_too_long`, `api_error`, `malformed_tool_use_exhausted` and `budget_exhausted`. | Corrected (the proposal listed three subtypes and no `terminal_reason`). [Agent SDK TypeScript reference](https://code.claude.com/docs/en/agent-sdk/typescript) |
| Claude Code | `continue: false` stops processing and takes precedence over event-specific decisions; `stopReason` is shown to the user. PreToolUse decisions are `allow`, `deny`, `ask`, `defer`, and `updatedInput` replaces arguments. | Corrected (`defer` is new). [Hooks reference](https://code.claude.com/docs/en/hooks) |
| Claude Code | SubagentStop runs when a subagent finishes, matched on agent type; `decision: "block"` with a `reason` keeps it running and delivers the reason as its next instruction. Stop hooks have an eight-consecutive-continuation cap that resets on each Tool call. | Verified, plus the cap detail. [Hooks reference](https://code.claude.com/docs/en/hooks) |
| Claude Code | API retries: `CLAUDE_CODE_MAX_RETRIES` defaults to 10 (capped at 15 from v2.1.186; `CLAUDE_CODE_RETRY_WATCHDOG` lifts it). Read-only Tools and subagents run in parallel (`CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY`, default 10). | Verified. [Environment variables](https://code.claude.com/docs/en/env-vars) |
| Claude Code | The internal query loop yields one typed terminal reason from a single function, and an aborted Tool returns to the model as an interrupted `tool_result`. | Unverified: closed source. Only the SDK surface (`terminal_reason`) is documented. |
| OpenCode | A runner step returns a closed tagged `Outcome`: `Completed`, `Retry`, `Continue`, `RecoverFull`, `Compacted`, `OutputLimit`. A retry is a separate `Decision { attempt, delay }`. | Corrected (the proposal described per-step finish reasons in a session processor; current main has this runner). [runner/step.ts:33-44](https://github.com/sst/opencode/blob/d85746a054924241a7edde736eefd079dff9bc00/packages/core/src/session/runner/step.ts#L33-L44), [runner/retry.ts](https://github.com/sst/opencode/blob/d85746a054924241a7edde736eefd079dff9bc00/packages/core/src/session/runner/retry.ts) |
| OpenCode | Retries: up to 10, exponential from 2 s capped at 10 s per gap, `Retry-After` honored up to 15 minutes, at most 3 timeout retries, on rate-limit, internal and transport errors, only before output starts. | Verified. [model-retry.ts:12-40](https://github.com/sst/opencode/blob/d85746a054924241a7edde736eefd079dff9bc00/packages/core/src/model-retry.ts#L12-L40), [ai/src/provider-error.ts:81-96](https://github.com/sst/opencode/blob/d85746a054924241a7edde736eefd079dff9bc00/packages/ai/src/provider-error.ts#L81-L96), [runner/step.ts:200](https://github.com/sst/opencode/blob/d85746a054924241a7edde736eefd079dff9bc00/packages/core/src/session/runner/step.ts#L200) |
| OpenCode | `doom_loop` is a V1 permission that **asks** (default `ask`) when the same Tool call repeats three times with identical input. It does not stop the Run. V2 Core no longer has it. | Corrected (the proposal and [tools-protocol D6](../tools-protocol/README.md#decisions) say "stop"). [V1 permissions:168, :177](https://github.com/sst/opencode/blob/d85746a054924241a7edde736eefd079dff9bc00/packages/web/src/content/docs/permissions.mdx#L168), [V2 permissions:138](https://github.com/sst/opencode/blob/d85746a054924241a7edde736eefd079dff9bc00/services/www/src/docs/content/permissions.mdx#L138) |
| OpenCode | Approval replies are `once`, `always`, `reject`. `/undo` restores conversation and files from snapshots. Agents cap model `steps`, then demand a text-only answer. | Verified. [V2 permissions:277-279](https://github.com/sst/opencode/blob/d85746a054924241a7edde736eefd079dff9bc00/services/www/src/docs/content/permissions.mdx#L277-L279), [snapshots](https://github.com/sst/opencode/blob/d85746a054924241a7edde736eefd079dff9bc00/services/www/src/docs/content/snapshots.mdx), [agents:280](https://github.com/sst/opencode/blob/d85746a054924241a7edde736eefd079dff9bc00/services/www/src/docs/content/agents.mdx#L280), [runner/max-steps.ts](https://github.com/sst/opencode/blob/d85746a054924241a7edde736eefd079dff9bc00/packages/core/src/session/runner/max-steps.ts) |
| Pi | Minimal loop: stream, run the Tool calls, append results, loop until none remain. Closed `StopReason`: `pending`, `stop`, `length`, `toolUse`, `error`, `aborted`, `deferred`. An `error` or `aborted` stop ends the Run. | Verified. [agent-loop.ts:245](https://github.com/badlogic/pi-mono/blob/73f8de80bc87021375f8b525292ac07492253002/packages/agent/src/agent-loop.ts#L245), [ai/src/types.ts:474](https://github.com/badlogic/pi-mono/blob/73f8de80bc87021375f8b525292ac07492253002/packages/ai/src/types.ts#L474), [how-pi-works.md](https://github.com/badlogic/pi-mono/blob/73f8de80bc87021375f8b525292ac07492253002/packages/coding-agent/docs/how-pi-works.md) |
| Pi | Sessions are a JSONL tree with `id`/`parentId` branches. There is no approval before Tool calls; a `tool_call` extension handler may mutate or block a call. There are no built-in subagents. | Verified. [session-format.md](https://github.com/badlogic/pi-mono/blob/73f8de80bc87021375f8b525292ac07492253002/packages/coding-agent/docs/session-format.md), [security.md](https://github.com/badlogic/pi-mono/blob/73f8de80bc87021375f8b525292ac07492253002/packages/coding-agent/docs/security.md), [extensions.md:201](https://github.com/badlogic/pi-mono/blob/73f8de80bc87021375f8b525292ac07492253002/packages/coding-agent/docs/extensions.md#L201), [README.md:19](https://github.com/badlogic/pi-mono/blob/73f8de80bc87021375f8b525292ac07492253002/packages/coding-agent/README.md#L19) |
| Pi | Agent-level automatic retry: three retries, 2 s base, 60 s maximum delay. | New fact. [settings.md:128-136](https://github.com/badlogic/pi-mono/blob/73f8de80bc87021375f8b525292ac07492253002/packages/coding-agent/docs/settings.md#L128-L136) |
| Goose | The agent runs a state machine: ordered operations over persisted conversation state. Each returns `NotApplicable` or `Applied(StepResult { effects, status })`, with closed `RunStatus` `Continuing`, `Yielded` or `Cancelled`. An `EffectHandler` applies the effects and the machine reloads state before the next step. | New fact (the proposal did not cover it); the closest peer to decide-then-apply. [goose-agent/src/machine.rs:121-226](https://github.com/block/goose/blob/fb9e3c89b2b4670a0eded99e7567e578ac6e502c/crates/goose-agent/src/machine.rs#L121-L226), [operation.rs:177-192](https://github.com/block/goose/blob/fb9e3c89b2b4670a0eded99e7567e578ac6e502c/crates/goose-agent/src/operation.rs#L177-L192) |
| Goose | `DEFAULT_MAX_TURNS` is 1000. `SmartApprove` asks only for sensitive calls and uses a model judge to detect read-only requests. Subagents exist. Provider retry defaults to 3. | Verified. [agents/agent.rs:52](https://github.com/block/goose/blob/fb9e3c89b2b4670a0eded99e7567e578ac6e502c/crates/goose/src/agents/agent.rs#L52), [goose_mode.rs:22-30](https://github.com/block/goose/blob/fb9e3c89b2b4670a0eded99e7567e578ac6e502c/crates/goose-provider-types/src/goose_mode.rs#L22-L30), [permission_inspector.rs:172-236](https://github.com/block/goose/blob/fb9e3c89b2b4670a0eded99e7567e578ac6e502c/crates/goose/src/permission/permission_inspector.rs#L172-L236), [retry.rs:14](https://github.com/block/goose/blob/fb9e3c89b2b4670a0eded99e7567e578ac6e502c/crates/goose-provider-types/src/retry.rs#L14) |
| Aider | `max_reflections = 3`, an architect/editor coder split and `/undo`. Provider retries back off for up to 60 s. | Verified (last commit 2026-05-22). [base_coder.py:101, :939](https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/coders/base_coder.py#L101), [architect_coder.py](https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/coders/architect_coder.py), [models.py:26](https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/models.py#L26) |
| OpenAI Agents SDK | Each step ends in a closed next step: `NextStepHandoff`, `NextStepFinalOutput`, `NextStepRunAgain` or `NextStepInterruption` (pending approvals). `DEFAULT_MAX_TURNS = 10` raises `MaxTurnsExceeded`. `RunState` serializes and `approve()` resumes. Model retries are opt-in (`ModelRetrySettings`). | Verified, plus `NextStep`. [run_internal/run_steps.py:164-190](https://github.com/openai/openai-agents-python/blob/64cd5dfc5a1579870dcbed0ea42b27aa84033fee/src/agents/run_internal/run_steps.py#L164-L190), [run_config.py:45](https://github.com/openai/openai-agents-python/blob/64cd5dfc5a1579870dcbed0ea42b27aa84033fee/src/agents/run_config.py#L45), [run_state.py:1380](https://github.com/openai/openai-agents-python/blob/64cd5dfc5a1579870dcbed0ea42b27aa84033fee/src/agents/run_state.py#L1380), [model_settings.py:188](https://github.com/openai/openai-agents-python/blob/64cd5dfc5a1579870dcbed0ea42b27aa84033fee/src/agents/model_settings.py#L188) |
| LangGraph | The default recursion limit is 10007, overridable by `LANGGRAPH_DEFAULT_RECURSION_LIMIT`. | Corrected (the proposal said 25). [_internal/_config.py:32](https://github.com/langchain-ai/langgraph/blob/1400d25b7b88cfa055c55b8a82bd95b7c25caef1/libs/langgraph/langgraph/_internal/_config.py#L32) |
| LangGraph | Durability is `sync`, `async` or `exit`; only `sync` persists before the next step. A resumed interrupt restarts its node and re-executes its logic. `RetryPolicy` defaults to 3 attempts. | Corrected (a checkpoint per superstep holds only under `sync`). [types.py:98-104, :918](https://github.com/langchain-ai/langgraph/blob/1400d25b7b88cfa055c55b8a82bd95b7c25caef1/libs/langgraph/langgraph/types.py#L98-L104), [RetryPolicy](https://github.com/langchain-ai/langgraph/blob/1400d25b7b88cfa055c55b8a82bd95b7c25caef1/libs/langgraph/langgraph/types.py#L427-L440) |
| ACP | v1 `StopReason`: `end_turn`, `max_tokens`, `max_turn_requests`, `refusal`, `cancelled`. v2 adds `Error(ErrorStopReason)`. Arany speaks v1 ([ACP](../../docs/specs/acp.md#behavior)). | Verified, plus v2. [v1/agent.rs:3408](https://github.com/agentclientprotocol/agent-client-protocol/blob/891600e47c19ff2dd6f4a8d5474a0f23a5a447ca/agent-client-protocol-schema/src/v1/agent.rs#L3408), [v2/agent.rs:3562](https://github.com/agentclientprotocol/agent-client-protocol/blob/891600e47c19ff2dd6f4a8d5474a0f23a5a447ca/agent-client-protocol-schema/src/v2/agent.rs#L3562) |

Peer behavior that does not survive the guarantees, and its replacement:

1. An unknown Tool outcome treated as an ordinary error the model may retry (Codex, Pi) becomes Uncertain, which stops the continuation and is reported unknown.
2. Re-execution on resume (LangGraph) becomes a committed intent and observation per effect; replay reads, never runs.
3. Unjournaled retries (all peers) become D8: every attempt is committed and charged.
4. Host-side hooks and parallel Tool calls (Claude Code, Codex) become Guard-hosted hooks and one Guard grant per effect; parallelism stays with next steps item 10.

## Target design

One private module, `engine/run_loop/stop.rs`, holds the stop vocabulary and settlement decision. Each other decision lives beside its shell function. Everything is `pub(super)`; nothing leaves the loop except `RunStop` (R7).

### Core types

```rust
// engine/run_loop/stop.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StopCause {
    Uncertain,                       // an Uncertain hook or Tool observation
    Cancelled,
    HookStop,                        // a settled hook journaled `stopped`
    Paused,
    ActiveTime,
    Budget,                          // Goal call or Run budget
    Capacity,                        // Store or Event admission (was Err(Tool(Limit)))
    ContextFull,
    DelegationDeadline,
    Refused(ProposalRefusalCause),   // tools-protocol slice 1 vocabulary
    ToolStopped,                     // denied, dropped or Guard failure that stops the Run
    InvalidCommandResult,
}

impl StopCause {
    const fn rank(self) -> u8;       // D2, highest first
    // The higher rank wins; on equal rank the earlier (self) is kept.
    pub(super) fn merge(self, later: Self) -> Self;
}

#[must_use]
pub(super) enum StepEnd { Continue, Stopped(StopCause) }

// Independent observations read once per decision from one ledger view and the token.
// They are facts, not decision state; decisions never live in them.
pub(super) struct RunFacts {
    cancelled: bool,
    paused: bool,
    hook_stop: bool,
    uncertain: bool,                 // any Uncertain hook or Tool observation in the Run
    hooks_settled: bool,
    active_time_exhausted: bool,
    budget_closed: bool,             // !can_start_provider_call() || remaining_calls == Some(0)
    tokens_within_limit: bool,
    tool_admitted: bool,             // admits(Action::Tool)
    settleable: bool,                // Goal open for this Run, no reservation, agents closed
    workers_settled: bool,
}

impl RunFacts {
    pub(super) fn observe(loop_: &RunLoop<'_>) -> Result<Self, EngineError>;
    pub(super) fn observed(&self) -> Option<StopCause>;   // merge of what the facts show
}

pub(super) enum PrimaryEnd { Finished(Finish), Stopped(StopCause) }

#[must_use]
pub(super) enum Settlement {
    Finish(Finish),        // AgentFinished, MessageCommitted, RunFinished: Finished
    Fail(RunFailure),      // Failed with its cause (present exactly with Failed)
    Cancel,                // Cancelled
    HookStop,              // StoppedByHook
    Unknown,               // no terminal Event; Err(ToolError::Uncertain) (D3)
    Unsettleable,          // no terminal Event; reported as RunStop::Uncertain since R7
}

pub(super) fn settle(end: PrimaryEnd, facts: &RunFacts) -> Settlement;
```

`Settlement` replaces the proposal's `Terminal { agent, run, failure, uncertain: bool }`. The primary's AgentRun and Run dispositions are equal on every current path, so one closed variant per outcome is enough, and "Failed without a cause" or "journaled and unknown" cannot be written.

### Decide signatures

```rust
// Primary, after a Provider call is recorded; also used by the wave primary branch.
pub(super) enum PrimaryDecision {
    Finish(Finish),
    Delegate(Delegation),
    ToolStep(ToolRequest),
    Refused(ProposalRefusalCause),   // tools-protocol slice 2: continue, or RefusalLimit
    Stop(StopCause),
}
pub(super) fn decide_primary_call(
    site: PrimarySite,               // Loop | Wave { workers_active: bool }
    phase: AgentPhase,
    result: &CallResult,
    validity: Result<(), ProposalRefusalCause>,
    facts: &RunFacts,
) -> PrimaryDecision;

// Dispatch precheck: the shell runs hooks and awaits any Ask, then decides once.
pub(super) enum NotDispatched {
    DenyRule, OutsideGrant, ModeDenied, HookDenied(HookDenial), OutsidePlan,
    TimeoutLimit { requested: u64, maximum: u64 }, ActiveTime, TokenBudget,
    Paused, ApprovalRefused, UnsavedRule, UnguardedNeedsHuman, HookFailureAfterClassify,
}
pub(super) fn decide_dispatch(
    intent: &EffectIntent,
    permission: Permission,          // after narrow_terminal and narrow_hooks
    hooks: &HookFold,
    approval: Option<&Resolution>,
    timeout_limit: Option<(u64, u64)>,
    facts: &RunFacts,
) -> Result<Dispatch, NotDispatched>;

pub(super) enum AdmissionRefusal { RemainingCalls, StoreCapacity }
// admit_tool: Result<Result<(), AdmissionRefusal>, EngineError>; StoreCapacity → StopCause::Capacity.

// Child wave, after a child's call joins.
pub(super) enum ChildDecision {
    ToolStep(ToolRequest, ProviderTurn),
    Finish(Finish, ProviderTurn),    // the shell runs SubagentStop, then decide_child_stop
    Reprompt(RepromptReason),        // closed; rendered once
    End(StopCause),
}
pub(super) fn decide_child_call(
    child: &ChildWork,
    result: &CallResult,
    validity: Result<(), ProposalRefusalCause>,
    gate: Option<GateFacts>,         // editors: criterion_unmet, gate_continuations
    queue_stop: Option<StopCause>,
    facts: &RunFacts,
) -> ChildDecision;

pub(super) enum ChildEnd { Finished { data: Option<WorkerDataRef>, no_change: bool }, Stopped(StopCause) }
pub(super) struct WorkerTerminal { agent: AgentDisposition, worker: WorkerDisposition, cause: WorkerCause }
pub(super) fn decide_worker_terminal(
    end: ChildEnd,
    private_uncertain: bool,
    criterion_unverified: bool,
    actor_hook_stop: bool,
    turns_left: u32,
    facts: &RunFacts,
) -> WorkerTerminal;

// Provider attempt (R8), shared by routing.rs and the child wave.
pub(super) enum RetryCause { RateLimited, ServerError, StreamCut }
pub(super) enum AttemptDecision {
    Accept,                                        // the result is the call's outcome
    Retry { cause: RetryCause, delay: Duration },  // same route, after a committed fact
    Switch,                                        // next configured fallback route
}
pub(super) fn decide_attempt(result: &CallResult, attempt: &AttemptFacts) -> AttemptDecision;
// AttemptFacts: retries used, server Retry-After, time left before the action deadline and
// active-time end, whether the chain has a next route, call-budget and token admission.
```

Each decide function has one implementation and one consumer, so none is a trait ([codebase-design](../../.agents/skills/codebase-design/SKILL.md): one adapter is a hypothetical seam). A closure would hide the inputs the table test must enumerate.

### Apply shell

```rust
impl RunLoop<'_> {
    async fn invoke_primary(&self, root: &mut ProviderRequest) -> Result<(CallResult, ActiveSegment), EngineError>;
    async fn apply_primary(&self, d: PrimaryDecision, root: &mut ProviderRequest) -> Result<StepEnd, EngineError>;
    async fn execute_step(&self, grant: Grant<'_>, request: ToolRequest, turn: ProviderTurn) -> Result<StepEnd, EngineError>;
    async fn apply_child(&self, d: ChildDecision, child: &mut ChildWork, queue: &mut ChildQueue) -> Result<ChildEnd, EngineError>;
    async fn settle_primary(&self, end: PrimaryEnd) -> Result<(), EngineError>;   // the only primary RunFinished writer
}

// The primary loop, whole.
loop {
    if let Some(cause) = self.boundary_stop()? { break self.settle_primary(PrimaryEnd::Stopped(cause)).await; }
    let (result, segment) = self.invoke_primary(root).await?;   // reserve, journal, call (R8 retries inside), settle usage
    let facts = RunFacts::observe(self)?;
    let decision = decide_primary_call(PrimarySite::Loop, root.phase, &result, self.validity(&result)?, &facts);
    self.append(Event::ProviderCallRecorded { record: call_record(&decision, ..) }).await?;   // journal before acting
    self.settle_active_segment(segment)?;
    match self.apply_primary(decision, root).await? {
        StepEnd::Continue => continue,
        StepEnd::Stopped(cause) => break self.settle_primary(PrimaryEnd::Stopped(cause)).await,
    }
}
// Every `?` above is infrastructure (journal, Store, coordinator); it is never a decision.
```

The child wave is the same loop with a `JoinSet` in front: join, observe facts, `decide_child_call`, `apply_child`, refill. A worker's end goes through `decide_worker_terminal` before its `AgentFinished`.

The result combines the best of the peers:

- Pi's one-step loop for the chat basics.
- A Codex- or Agents-SDK-shaped closed value for every exit.
- Claude Code's permission, hook and subagent vocabulary, hosted in the Guard.
- Goose's persist-then-reload discipline, made strict: the journal is the state, and each step's outcome commits before the next step.

### Unchanged

- The `execute_step` pipeline body (sized intent, committed intent, approval, Guard dispatch, committed observation, PostToolUse) owns per-effect isolation; only its return type changes.
- `GoalLedger`: `RunFacts::observe` reads through `ledger.view` and adds no ledger API.
- Reducer validation (`reducer/agent.rs:640-690`, `reducer/run.rs:200-245`): fix the mapping, never the validator.
- `Grant::{Primary, Researcher}`: a real seam with two adapters.
- HookFold.
- `Box::pin` boxing (#132 measured stack use).
- Provider routing beyond the attempt decision.

Deferred until R1-R4 show remaining pain:

- An RAII `ActiveSegment`.
- A typed `ChildQueue` state machine.
- A repeated-call stop. OpenCode's V1 doom loop asks and V2 dropped it, so no peer stops on repetition.

## Contract and scenarios

R1, R2, R3a, R4, R5 and R6 restore or preserve specified behavior. Their journals stay byte-identical, apart from mappings the R2 table makes explicit, and they need no spec edit unless R2 surfaces a disagreement the owner accepts. R3b, R7 and R8 change observable behavior:

- **R3b.** [Execution](../../docs/specs/execution.md#worker-journal-protocol) already specifies most worker causes. Add one sentence only for a cause it does not name.
- **R7.** [Serve](../../docs/specs/serve.md#notifications-and-items) (`goal.stopped` becomes closed), [ACP](../../docs/specs/acp.md#errors-and-envelopes) (the mapping table) and [Goals](../../docs/specs/goals.md#current-format-replay-and-exec) (`exec` stop line and exit status).
- **R8.** [Provider](../../docs/specs/provider.md#usage-provenance-and-reliability) replaces "zero same-route automatic retries"; [execution](../../docs/specs/execution.md#admission-persistence-and-recovery) adds the retry fact and its replay rule. The [system overview](../../docs/architecture/system-overview.md#5-session-run-and-collaboration-lifecycle) and [execution accepted outcomes](../../docs/specs/execution.md#accepted-outcomes) sentences are updated to link instead of restating.

Scenarios that every slice keeps green: a cancel during a Tool, a cancel during SubagentStop (the #166 case), a hook stop with workers pending (the #129 blocker), an active-time stop during a hook, Store capacity during a worker step, an Uncertain observation in the primary and in a worker, pause at a boundary, budget exhaustion on the last call, and crash after each committed Event, followed by strict replay.

## Execution

Prerequisite: tools-protocol slices 1, 2, 4 and 5 merged (slice 3 landed as #183). R1 starts from the merge commit of slice 5. Each slice is one stacked PR ([Git](../../agents/git.md#pull-requests)) and applies [Ponytail](../../.agents/skills/ponytail/SKILL.md), [rust-clean-code](../../agents/skills/rust-clean-code/SKILL.md) and [clean-architecture-review](../../agents/skills/clean-architecture-review/SKILL.md).

Order (D10): tp1 → tp2 → tp3 → tp4 → tp5 → R1 → R2 → R3a → R3b → R4 → R5 → R7 → R6. tp6, tp7 and R8 interleave after tp5; R8 lands before R4.

### R1: `StopCause` and `StepEnd` (mechanical)

- **Scope.**
  - Add `stop.rs` with `StopCause`, `rank` and `merge`, and `StepEnd`.
  - `execute_step`, `run_intent_inner`'s `Option` return and `sized_intent` produce the cause at the site that knows it. Inside a step, `Err(GoalBudgetExhausted)` becomes `Stopped(ActiveTime)` and Store admission `Err(Tool(Limit))` becomes `Stopped(Capacity)`.
  - Callers keep their ladders but match the cause instead of re-reading the ledger.
  - Delete tools-protocol's `tool_stop_cause()`.
- **Tests.** Existing `engine_run`, `guard_runs` and replay corpora unchanged. One table in `stop.rs`: the sizing refusals map to `ActiveTime` or `ContextFull`, and `merge` is associative, idempotent and rank-monotone.
- **Guarantees.**
  - Journal-first: unchanged order; no Event changes.
  - Strict replay: byte-identical journals.
  - Honest uncertainty: Uncertain gets the top rank, but settlement still goes through today's code until R2.
- **Specs.** None.
- **Risk.** Low to medium: 37 return sites, each a one-line hunk. Review with `git diff -w` against a checklist of the ten `execute_step` sites.

### R2: `RunFacts`, `settle`, `settle_primary` and `decide_primary_call` (one author)

- **Scope.**
  - Replace `Exit` (`run_loop.rs:474-481`) with `PrimaryEnd`.
  - Collapse `settle_stopped_primary`, `fail_primary`, `fail_for_budget`, `finish_hook_stop`, `cancel_primary`, `settle_definition_stop`'s branch and `finish_primary`'s pre-checks into `settle_primary` over `settle()`. `mark_budget_stopped()` becomes the `Fail(BudgetExhausted)` branch.
  - `decide_primary_call` serves both the loop and the wave primary (`children.rs:707-829`), which ends the duplicated validity check.
  - Delete `DefinitionStop`, keeping its cases as rows.
- **First commit.** A table in the PR body lists the precedence each deleted site used and the D2 result. Every row where they differ is a behavior change the owner accepts before merge.
- **Tests.** A table over `settle()`: every `StopCause` × {cancelled, hook stop, uncertain, budget closed, settleable} → `Settlement`. A table over `decide_primary_call`: site × outcome × facts. Existing journeys: `guard_runs/permissions.rs` hook stops, `engine_run/goals.rs` budget stops, the cancellation cases.
- **Guarantees.**
  - Journal-first: `settle_primary` appends `AgentFinished`, then `MessageCommitted` (Finish only), then `RunFinished`, as today.
  - Strict replay: no payload bump; tools-protocol's `RunFailure` covers every cause.
  - Honest uncertainty: `Unknown` journals nothing and returns `Err(ToolError::Uncertain)` on every path (D3).
- **Specs.** None, unless the precedence table changes an observable outcome; that sentence then goes to [execution accepted outcomes](../../docs/specs/execution.md#accepted-outcomes).
- **Risk.** Medium. Every disagreement between today's sites is a behavior change by definition, and the table makes each one explicit.

### R3a: `decide_child_call` and `decide_worker_terminal` (one author)

- **Scope.**
  - Delete `AbortReason`. `Drained.abort` and `ChildQueue.stop` become `Option<StopCause>`, which replaces `budget_stopped`, `refusal` and `deferred_at`.
  - `Reprompt(RepromptReason)` replaces the prose at `children.rs:806-819`.
  - Both worker ladders (`worker.rs:855-913`), `settle_child_call`'s ladder, the pending drain in `settle_delegation_inner` and `hook_abort` go through the two decide functions.
  - Editor SubagentStop (D7) flows through `ChildDecision::Finish`, then the shell's `subagent_stop`, then `decide_child_stop`.
- **Byte rule.** Journals are byte-identical: `decide_worker_terminal` reproduces today's causes, including the generic ones R3b fixes.
- **Tests.** Tables over both decide functions. Team journeys `hook_sibling_stop` and `hook_uncertain_sibling` (`guard_runs/worker.rs`), plus the existing editor SubagentStop reprompt case.
- **Guarantees.**
  - Journal-first: `WorkerStopObserved` still commits before further team decisions.
  - Strict replay: byte-identical.
  - Honest uncertainty: an uncertain hook becomes `StopCause::Uncertain` instead of `Failed`, but the journaled disposition stays what the reducer accepts today. Unknown private work still stops the actor permanently.
- **Specs.** None.
- **Risk.** Medium-high: the widest structural diff. Run the full `guard_runs` worker journeys on Linux and macOS CI before merge.

### R3b: worker cause relabel (one author, revertible)

- **Scope.** In `decide_worker_terminal` only, map ToolFailed to TurnLimit, InvalidOutcome, StoppedByHook or Cancelled where the stop says so. A refused SubagentStop reprompt gets `budget` (deadline, call or token budget) or `turn_limit` (turns alone). This closes the `followups.md` items for #129, #130 and #151.
- **Tests.** New table rows; `session/reducer` worker pairing rows for each produced cause; the replay corpus for worker Runs.
- **Guarantees.**
  - Journal-first: unchanged order.
  - Strict replay: the reducer's reason-to-disposition pairing (`events/records.rs:535-570`) must accept each new cause, so a wrong mapping fails replay rather than journaling silently. The variants exist already, so no payload bump.
  - Honest uncertainty: unaffected.
- **Specs.** [Execution worker journal protocol](../../docs/specs/execution.md#worker-journal-protocol): one sentence per cause it does not name yet.
- **Risk.** Medium. It reverts alone.

### R4: split `drive_children_inner` (mechanical)

- **Scope.** The body becomes join, classify, `decide_child_call`, apply, refill. The fallback re-spawn block (`children.rs:645-703`, or its R8 form) becomes one function shared with `routing.rs`. The gate-continuation condition becomes `editor_may_continue_after_gate`. Remove `#[expect(clippy::too_many_lines)]`.
- **Tests.** Existing journeys only.
- **Guarantees.**
  - Journal-first: `fill_slots` keeps its position relative to settlement (#148). Extract functions that take and return the queue; never reorder calls.
  - Strict replay and honest uncertainty: unchanged.
- **Specs.** None.
- **Risk.** Medium: ordering is subtle.

### R5: `decide_dispatch` and `Admission` (one author)

- **Scope.**
  - `before_dispatch` and `before_primary_dispatch` become shell plus one pure decision. `NotDispatched` is rendered once by `not_dispatched(intent, reason)`, which replaces the three mutable bools and six prose strings.
  - `admit_tool` returns `Result<Result<(), AdmissionRefusal>, EngineError>`, so Store capacity is never `Tool(Limit)` and the worker-only remap goes.
- **Shell order.** Hooks, then the Ask, then the decision, then the pause reservation (its failure is the second, small decision: `ActiveTime`), then `ToolPermissionDecided`, then dispatch.
- **Tests.** A table over `decide_dispatch`; `guard_runs/permissions.rs` journeys; presentation snapshots unchanged byte for byte.
- **Guarantees.**
  - Journal-first: the verdict commits before dispatch, as today.
  - Per-effect isolation: the Guard path is untouched.
  - Strict replay: byte-identical observations.
  - Honest uncertainty: unaffected.
- **Specs.** None.
- **Risk.** Medium. It must follow tp4, which changes `plan_allows`, an input of `decide_dispatch`.

### R7: closed `RunStop` at the edges (one author)

- **Scope.**
  - `RunResult.stopped: Option<RunStop>` with `RunStop::{GoalExhausted(GoalLimit), Paused, Cancelled, StoppedByHook, Uncertain, Failed(RunFailure)}` and `GoalLimit::{Calls, Runs, ActiveTime, Tokens}`.
  - One pure projection from committed Goal and Run views computes it for live results and historical turns alike.
  - `exec`, `serve` and `acp` map it in their adapters. `serve`'s `goal.stopped` text and `items.rs:141-150` re-derivation go, and ACP's mapping uses the projection.
  - `Settlement::Unsettleable` returns its `RunStop` instead of `Err(GoalBudgetExhausted)`.
- **Tests.**
  - Serve and ACP in-process conformance rows for every variant.
  - `exec` exit-code and stop-line tests.
  - Replay: a historical turn projects the same `RunStop` as the live result.
- **Guarantees.**
  - Journal-first and strict replay: the projection reads only committed facts.
  - Durable Goals: exhaustion is a typed fact at every edge.
  - Honest uncertainty: `Uncertain` reaches each edge (ACP keeps its error path and `arany.uncertainEffects`).
- **Specs.** Serve "Notifications and items" (`goal.stopped` becomes closed; schema export changes), the ACP mapping table, and Goals' `exec` row.
- **Risk.** Medium: a public protocol change. Pre-beta it has one current format and no compatibility reader.

### R6: idiom sweep (mechanical)

- **Scope.**
  - `#[must_use]` on decision types.
  - Exhaustive matches without `_` over `StopCause`, `WorkerCause` and `Settlement`.
  - The remaining `Ok(false)` decisions in `initialize_definitions` (`step.rs:2107`) and `review_tool` (`:1819`) become typed.
  - A `RunFacts` test builder in the engine tests.
- **Tests.** Clippy and the unit tables.
- **Guarantees.** Unchanged; byte-identical.
- **Specs.** None.
- **Risk.** Low.

### R8: bounded, journaled Provider retries (one author)

- **Scope.**
  - Adapters classify 429 (with `Retry-After`), 5xx before body admission and a stream cut before a decoded outcome as retryable `CallFailure`s.
  - `decide_attempt` (D8) replaces the `eligible: bool` in `routing.rs:365-394` and drives both attempt loops.
  - Before each retry the shell commits a bounded retry fact (attempt, closed cause, delay), then makes a fresh Provider-call reservation. That mirrors `ProviderFallbackUsed`, which commits before a switch's replacement dispatch.
  - A provisional preview is cleared and the retry is shown, as Codex's `Reconnecting... n/max` is.
  - After the last retry the existing fallback switch applies, then the ordinary failure cause.
- **Tests.**
  - Engine corpus with the scripted Provider: a 429 then success (one retry fact, two charged calls), three cuts then failure (`provider_call` cause), and a retry refused by a finite token budget with unknown usage.
  - Cancel during the backoff (no further reservation), and crash after the retry fact, then replay.
  - Reducer rows for the new fact.
- **Guarantees.**
  - Journal-first: the retry fact commits before re-dispatch.
  - Strict replay: replay validates that the attempt count is within the bound and that the reservation and budget fit; it never re-sends.
  - Durable Goals: each attempt is charged, and the backoff counts as active time.
  - Honest uncertainty: no Workspace effect repeats, because nothing was decoded or committed.
- **Payloads.** A new Event or a bumped `ProviderCallRecorded`; the author picks the smaller under one current payload version, with check versions bumped.
- **Specs.** [Provider](../../docs/specs/provider.md#usage-provenance-and-reliability) "Usage, provenance and reliability", [execution](../../docs/specs/execution.md#admission-persistence-and-recovery) (the fact and its replay rule) and [ChatGPT](../../docs/specs/chatgpt.md#catalog-checks-and-runs) if its stream errors classify differently.
- **Risk.** Medium: plan quota spent on retries. The bound is three, and each retry is visible.

## Security and resource model

No trust boundary moves. Decide functions read committed facts and closed enums only, never model text (safe AI cage), and `Refused` carries only the closed tools-protocol cause.

[Security-pass triggers](../../agents/security.md#security-pass-triggers):

- R7: protocol output to editors and clients.
- R8: Provider egress and retry timing; a hostile `Retry-After` is capped by the action deadline.

Bounds:

- Retries: three per semantic call, gaps of at most 10 s unless the server asks for longer, and never past the action deadline or active time.
- The retry fact is a fixed-size record.
- `RunFacts` is one view read per decision, with no allocation that scales with history beyond what `ledger.view` already does.
- No queue, buffer or concurrency limit changes.

## Risks and mitigations

- **Pure functions that callers bypass.** Extracted logic is safe only when no inline copy stays behind. Mitigation: each slice deletes the ladders it replaces in the same PR; R6's exhaustive matches catch stragglers.
- **Hidden behavior changes in R2 and R3a.** Mitigation: the precedence table in the PR body, byte-identical journal checks on the replay corpus, and R3b isolated so it reverts alone.
- **Rebase churn against the tools-protocol slices.** Mitigation: R1 starts only after tp5 merges; the two efforts never edit the same functions at once.
- **Retry cost.** Mitigation: the D8 bound, charging, and visible retry rows; the owner can still lower the Goal call budget.
- **Stack growth from new async helpers.** Mitigation: keep `Box::pin` at today's sites; R4 may only shrink the largest future.
- **Rollback.** Revert the slice. R3b and R8 journals reject on a reverted binary without changing their bytes; recover with the [explicit history reset](../../docs/specs/terminal.md#setup-and-account-choices). This is accepted pre-beta practice.

## Open questions

- R8 Event shape. Decided in R8: a new fixed-size `ProviderRetryScheduled` Event at payload version 1. A field on `ProviderCallRecorded` cannot commit before re-dispatch, because that record closes the call.

## Exit criteria

- All nine slices merged with green CI on Linux and macOS. The R2 and R3a precedence tables are accepted in their PRs.
- Each decide function has its table test. `execute_step` returns `StepEnd`. No `AbortReason`, `DefinitionStop`, `budget_stopped`, settlement ladder or `Err(GoalBudgetExhausted)` remains as intra-loop control flow, and `drive_children_inner` has no length allowance.
- Failure observations: the scenarios under [Contract and scenarios](#contract-and-scenarios) pass, including Uncertain with no terminal Event and strict replay after a crash at each committed Event.
- `exec`, `serve` and `acp` report the same `RunStop` for a live result and for its replayed history.
- A 429 followed by success shows one retry row and two charged calls in `arany show`. Live confirmation against ChatGPT and the API routes goes to [NEXT_STEPS](../../NEXT_STEPS.md) until stable pre-beta.
- Specs named per slice describe the current behavior.
- Closeout under the [planning lifecycle](../README.md#lifecycle): delete this plan and point the link in [library practice](../../agents/arany.md#engine-loop-decisions) to the owning tests instead.
