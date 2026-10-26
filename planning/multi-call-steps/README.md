# Multi-call Tool steps

## Metadata

- Status: `In progress`
- Owner: harness maintainers
- Last updated: 2026-10-26

## Outcome and scope

One capability: a primary or researcher model step may request up to four Tool calls; read-only calls dispatch concurrently, effectful calls dispatch in model order, and every call is journaled as its own committed intent and observation before the next Provider call. The harness responsibility served is the agent loop ([priority 1](../../AGENTS.md#product-and-implementation-priorities)) and the [journal-first](../../AGENTS.md#non-negotiable-guarantees) guarantee, which is preserved: N intents commit before any dispatch.

This plan absorbs two `NEXT_STEPS` items: "Multi-call steps (Stage D)" and "ChatGPT constant tool list"; both are deleted from `NEXT_STEPS.md` in the same change that adds this plan. Excluded: nested delegation, worker (editor) multi-call steps, and any change to the four-call cap.

## Research

S1–S3 are implemented: [Engine validation](../../crates/arany/src/engine/run_loop.rs), [Anthropic encoding/decoding](../../crates/arany/src/provider/anthropic/wire.rs) and [concurrent reads](../../crates/arany/src/tools.rs). OpenAI still sends `parallel_tool_calls: false` in [wire.rs](../../crates/arany/src/provider/openai/wire.rs); S4 remains. Implementation status is separate from passing-check evidence.

- The existing [step cap](../../docs/specs/limits.md#max_step_tool_calls), [multi-call Event ordering](../../docs/specs/execution.md#committed-event-stream) and [Provider encoding](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding) remain authoritative for S4.
- In-process reads already have a concurrency bound: "at most 16 concurrent exact Read, exact-file Search and Skill calls per process" ([tools.md Limits](../../docs/specs/tools.md#limits-and-failure-behavior)); the facts table marks `Site::InProcess` for the File family and researcher-eligible operations ([tools/catalog.rs](../../crates/arany/src/tools/catalog.rs)).
- Prompt caching: OpenAI routes send the Session UUIDv7 as `prompt_cache_key` ([openai/wire.rs:377,492](../../crates/arany/src/provider/openai/wire.rs)); only the API route uses the constant function list ([provider.md Prompt caching](../../docs/specs/provider.md#prompt-caching)).
- Peers: Codex sends `parallel_tool_calls: true` and serializes only tools not flagged parallel-safe through a per-runtime read/write lock ([codex-rs/core/src/tools/parallel.rs](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/parallel.rs)); handlers run in `tokio::spawn` and results are collected in order after the stream ends. Claude Code runs "Read-only tools (Read, Glob, Grep, and MCP tools marked as read-only)" concurrently and state-changing tools sequentially ([agent loop](https://code.claude.com/docs/en/agent-sdk/agent-loop#parallel-tool-execution)). Neither peer fails a turn for too many calls; an over-limit batch is simply executed. Anthropic's API returns several `tool_use` blocks in one `tool_use` stop and expects one `tool_result` per `tool_use_id` in the next user message; OpenAI Responses returns several `function_call` items and expects one `function_call_output` each.

## Contract and scenarios

- Owning specs: [provider.md Structured outcomes](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding) (encoding, replay of paired items), [execution.md Accepted outcomes](../../docs/specs/execution.md#accepted-outcomes) and [Committed Event stream](../../docs/specs/execution.md#committed-event-stream) (per-call Events, order), [tools.md In-process reads](../../docs/specs/tools.md#in-process-file-operations) (concurrency bound), [goals.md Lifecycle, budgets and recovery](../../docs/specs/goals.md#lifecycle-budgets-and-recovery) (per-call reservation).
- S4 preserves the [existing Engine validation, journal ordering and stop semantics](../../docs/specs/execution.md#committed-event-stream); adapter changes retain one correlated result per call in model order under the [Provider contract](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding).
- Over-cap response (five or more calls): refused through the proposal-refusal path with cause `InvalidArguments` and feedback naming the cap, counted against the existing identical-pair refusal cap. This replaces the `NEXT_STEPS` wording "fails the Run without retry": both peers continue the turn, and the existing refusal mechanism already bounds repetition; failing a Goal for a count the model can correct is not a guarantee.
- Scenarios: four concurrent Reads produce four committed pairs and one batched result; a Read plus Edit step dispatches the Edit only after the Read observation commits; cancellation between the second and third dispatch yields two observations and two `not_dispatched` records; a crash after two `ToolStarted` commits replays as two Interrupted intents and never dispatches the rest; replay of a four-call step validates `requested_tools = 4` and four correlated pairs; an over-cap response is refused with feedback; a researcher with two calls remaining that requests three is refused for scope.
- Evidence owners: `crates/arany/tests/engine_run/goals.rs` and `permissions.rs` (scripted multi-call responses), `crates/arany-cli/tests/session_run/tools.rs` and `tools/replay.rs` (replay and `show`), `crates/arany/src/provider/snapshots/` request byte snapshots per adapter, `session/reducer/tests.rs` for correlation and ordering.

## Chosen direction

- Simplest sufficient solution: retain the existing bounded `ProviderOutcome::Tools` and dispatch; extend only OpenAI decoding and request construction, with byte-pinned snapshots. No new Tool trait or scheduler.
- OS impact: none; concurrency is in-process reads only. Guard dispatch stays serial.

## Security and resource model

Concurrency is limited to attested in-process reads under the existing 16-slot bound and the existing per-call result budget; context reservation sums the four result budgets before dispatch (the existing per-step reserve times the call count, bounded by the Tool context allowance). Effectful calls never overlap each other or any read. Model-ordered validation before any commit prevents a partially committed step from widening scope. No new trust input.

## Risks and mitigations

- Four full result budgets (256 KiB) may not fit the remaining Tool context → the step is refused for `ContextFull` before any commit, exactly as a single oversized reservation is today; the model is told the available call count.
- ChatGPT phase-dependent lists keep busting the cache → slice 4 sends the constant list; the change is wire-only and snapshot-pinned.

## Vertical slices

**S4: OpenAI API and ChatGPT constant tool list with parallel calls.** `parallel_tool_calls: true` on the API route; decode several `function_call` items; constant function list across planning and synthesis with `allowed_tools` mode `auto`, and `tool_choice: "none"` for a spent researcher, on both OpenAI routes keyed by the Session UUIDv7 `prompt_cache_key`. Specs: `provider.md` prompt caching and structured outcomes; `chatgpt.md` request shape. ≤500.

## Exit criteria

- Scripted and snapshot evidence: a four-Read step on each route produces four committed pairs, one batched result message, byte-pinned request bodies, and identical `show` output after close/reopen (owners above).
- Failure observations: over-cap refusal with feedback; cancellation mid-step; crash after partial commit replays Interrupted without dispatch; `ContextFull` refusal before any commit.
- Contract updates in `provider.md`, `execution.md`, `tools.md`, `chatgpt.md` land in the slice PRs; `NEXT_STEPS.md` items "Multi-call steps (Stage D)" and "ChatGPT constant tool list" are removed when this plan is added and never re-added. Delete this folder when slice 4 lands.

## Gates

S4 edits OpenAI wire/subscription and may touch Provider registration/credentials. Wait for [architecture-residue R1a–R1c](../architecture-residue/README.md#r1--finish-p4s-one-way-diagnostic-boundary) to release wire diagnostics, and for [R5](../architecture-residue/README.md#r5--p5-native-name-residue) to release overlapping Provider files. Re-inventory after integration and preserve the current multi-call contract and snapshots.
