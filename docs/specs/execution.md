# Run execution

## Scope and related contracts

This contract owns bounded Run coordination, delegation, accepted outcomes and history-capacity rejection. [Tools](./tools.md) owns grants and effect continuations; [terminal](./terminal.md) owns attached controls and agent inspection. Session lifecycle and account/profile admission remain in the [Session](../../agents/session.md) and [Provider](../../agents/provider.md) contracts. [Engine](../../agents/engine.md), [Store](../../agents/store.md) and [security](../../agents/security.md) rules retain implementation and privilege constraints.

## Collaboration

Each accepted task has one accountable primary AgentRun and an ordered, bounded collection of direct read-only children. Children reason over supplied data and cannot create nested teams, acquire Tools or mutate files. The primary obtains file evidence, performs authorized effects and owns the final answer.

Pin one policy before the Run: `single` admits no children; default `auto` allows up to three total direct children when useful; explicit `team` requires decomposition on the first non-Tool outcome. The persisted `max_active_children` field limits total direct children for the delegation. Policy, process safety and remaining call/token/byte/time budgets bound team size; the Provider concurrency ceiling separately bounds simultaneous calls while admitted children wait in order. A ceiling is permission, not a required team size.

Auto guidance asks the primary to keep routine edits and simple inspection local, delegate substantial independent work only when its benefit justifies extra calls, and choose the smallest useful team. Typed policy and budgets enforce capacity; guidance does not guarantee every model's judgment. Agent inspection distinguishes the current/last Run from historical agents rather than presenting accumulated history as simultaneous activity.

Child identities and assignments retain ordinal order even when calls finish out of order. Synthesis receives results in assignment order after all terminal dispositions. Child failure drains siblings before primary failure. Cancellation reaches every admitted child and preserves already observed calls and terminal facts.

## Accepted outcomes

Every step produces exactly one phase-allowed semantic Finish, Delegate or enabled Tool proposal. A primary can continue through sequential Tool observations in both planning and synthesis. Children receive Single policy and no Tool context. Unknown operations, invalid arguments, unavailable catalog names and policy-incompatible outcomes reject without repair or an alternate encoding.

Tool-enabled OpenAI and ChatGPT primaries use strict native functions with required selection and parallel calls disabled; exactly one complete call is accepted. Final text, unknown/multiple/incomplete calls reject. Read-only requests, children, compaction and Anthropic retain their admitted structured-text encoding. Exact custom profiles reject Tool context before HTTP until tool-capable conformance is supported.

Structured-text Responses decoding joins ordered text parts exactly and requires one final structured outcome. Explicit commentary can precede it. The legacy absent/null phase is accepted only for an unambiguous single final. Refusal, malformed content, unknown phases, duplicate/ambiguous finals, late commentary and contradictory error/incomplete markers reject. Missing or invalid fields cannot be repaired by a later valid field.

All adapters reject reported input above the shared input ceiling and output above the exact request cap. Native OpenAI may retain unknown usage; custom and ChatGPT outcomes require positive labeled usage. Native API-key/custom output caps are remote request controls; ChatGPT has a local acceptance cap and cannot guarantee remote generation or plan consumption. A Tool review uses its own 256-token cap, rather than the ordinary final-answer cap.

The ChatGPT route uses one bounded SSE parser regardless of absent or misleading media labels. Optional event labels must agree with JSON types. A matching created/completed identity plus complete indexed item events may supply empty terminal output; populated terminal output must agree. Deltas never supply accepted results. Missing/incomplete closure, invalid identity/model/usage, contradictory items, encoding errors and overflow publish no partial outcome. Native/catalog media admission remains unchanged; no JSON/HTML fallback is implied.

Malformed/non-object/duplicate-key MCP arguments and selected-credential reflections, including nested escaped arguments, reject before persistence. Provider failure remains the call's outcome even when an earlier Tool denial could otherwise be continued. Every route has zero automatic retries or billing/model/encoding fallback. A lost response can still consume remote usage.

## Admission, persistence and recovery

Admit the selected Provider and complete bounded request before Workspace disclosure. Pin the Workspace, instructions/includes, policy, model/effort and caps for the Run. Commit accepted Message, Run and primary identity before inference; commit each observed call before its terminal AgentRun disposition. Known call usage survives an interrupted journal prefix.

Before Workspace input, inference or Tool preparation, require logical Event capacity for the entire bounded topology and configured continuations, plus conservative database-byte headroom. The Session and fork ancestors remain locked for the operation. Independent lineages remain concurrent; byte preflight does not reserve physical disk or guarantee against storage faults. Reject capacity exhaustion before disclosure or usage and direct the user to a new Session.

Fork admission validates the resulting lineage's depth and total Event count atomically; a failure leaves no partial target. Replay resolves at most eight lineage links and 10,000 Events and verifies exact inherited prefix digests. The direct Session has at most 64 lifetime compaction attempts, including failures. Inherited summaries do not consume a fork's direct quota. Reject the next attempt before inference or an attempt Event. Failed compaction preserves the last good summary and canonical history.

Tool attempts/results are untrusted historical facts. Replay, resume, fork and compaction never execute old intents or restore permission. Pre-dispatch Guard failure stops as Failed without effects; once the GO write begins, missing completion is Uncertain. Uncertain/cancelled effects stop without another call, retry or rollback claim. A fresh task may inspect current files and propose a newly authorized edit with fresh preconditions.

Cancellation before Message acceptance creates no Run. After admission it drains in-flight work and commits cancellation without a final answer, preserving prior facts. A terminal commitment already started wins its race with cancellation. Store failure leaves an interrupted prefix rather than manufacturing success or a cancellation receipt.

## Acceptance scenarios and evidence owners

| Scenario | Required observation | Existing owner |
|---|---|---|
| Single, bounded auto/team and failed/cancelled children | Policy admission, ordered synthesis, complete draining and closed replay agree | [Engine Run corpus](../../tests/engine_run.rs), [child coordination](../../src/engine/run_loop/children.rs) |
| Routine edit guidance and historical agent inspection | Shared prompts discourage unnecessary delegation; inspection separates Run and history counts | [Shared provider requests](../../src/provider.rs), [terminal frames](../../src/terminal/view.rs), [slash journey](../../tests/session_run/slash_completion.rs) |
| Strict text/native function outcomes and usage caps | Malformed/ambiguous/oversized outcomes reject; exactly one admitted result persists | [Native wire corpus](../../src/provider/openai/wire/tests.rs), [Engine Run corpus](../../tests/engine_run.rs) |
| Sparse/full SSE completion and invalid closure | Reconciliation accepts only complete matching items; failure publishes no partial answer | [Offline ChatGPT journey](../../tests/session_run/setup/offline_https/chatgpt/checked_turn.rs) |
| Event/byte headroom, resulting fork quota and compaction exhaustion | Rejection precedes disclosure, usage or partial target; previous history/summary remains readable | [Store](../../src/store.rs), [replay](../../src/store/replay.rs), [Engine Run corpus](../../tests/engine_run.rs), [automatic compaction journey](../../tests/session_run/auto_compaction.rs) |
| Sequential effects, minimum receipt budget and interrupted/resumed history | Correlated bounded facts persist; fresh work never redispatches a historical effect | [Real native Tool journeys](../../tests/session_run/tools.rs), [Tool corpus](../../src/tools/tests.rs) |

These are evidence owners, not claims that every native platform or live model passed. Dated results and remaining user checks belong to [next steps](../../NEXT_STEPS.md), the active [beta record](../../planning/arany-beta/README.md) and scoped security/review records.
