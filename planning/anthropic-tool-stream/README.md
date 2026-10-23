# Anthropic Tool turns name their failure stage and succeed on explicit-effort models

## Metadata

- Status: `Ready` for S1 and S2; S3 is blocked on the owner's live re-run after S1 (the cause below is ranked, not confirmed)
- Owner: `TBD`
- Last updated: `2026-10-23`

## Outcome and scope

A native Anthropic Messages Run that needs a Tool (for example "read AGENTS.md") completes its first `tool_use` step on an admitted or explicit-effort model such as `claude-haiku-5-5`, and every rejected Messages stream or decoded response names a closed failure stage: a non-null `ProviderCallRecorded.failure_reason` in the journal and a `messages stage=…` line in `development.log`, copying the ChatGPT `subscription stage=` pattern. Serves the harness responsibility of durable, honest Provider execution ([AGENTS.md](../../AGENTS.md#non-negotiable-guarantees): journal-first durability, honest uncertainty).

Out of scope: parallel `tool_use` (probe P2 in [NEXT_STEPS.md](../../NEXT_STEPS.md)), the consumer-subscription route, and changing the Tool catalog shape beyond what the confirmed cause needs.

## Research

### Observed failure (owner, live, 2026-10-23)

- Run A (`hola`) finished: `messages_end_turn_storage_unspecified`, 18,340 input tokens, 18,336 cache-creation. The request path, model admission, auth, SSE framing, `message_start` model equality, usage merge and `end_turn` decoding all work for this model.
- Run B (read two files, phase `root_plan`) failed after 1.2 s: `ProviderCallRecorded {disposition: invalid_response, failure_reason: null, usage: null, response_id: null, wire_provenance: null}`; `development.log`: `provider phase=RootPlan disposition=InvalidResponse reason=None`.

### What the record shape proves

- `failure_reason: null` with `invalid_response` is produced only by an error whose `failure_reason()` is `None` in the `InvalidOutcome` class ([`crates/arany/src/provider.rs:1065-1090`](../../crates/arany/src/provider.rs) and [`:1092-1119`](../../crates/arany/src/provider.rs); [`crates/arany/src/engine/run_loop.rs:1354-1365`](../../crates/arany/src/engine/run_loop.rs)). In practice that is `ProviderError::InvalidOutcome`. Every stream-decoder rejection in [`crates/arany/src/provider/anthropic/wire.rs:88-331`](../../crates/arany/src/provider/anthropic/wire.rs) returns `InvalidStream`, which records `stream_protocol` ([`provider.rs:1068`](../../crates/arany/src/provider.rs)). So the SSE/event decoder did **not** reject Run B; the NEXT_STEPS note attributing the null reason to the stream decoder's single `InvalidStream` is not consistent with the record. HTTP 4xx (including a request-side 400) records `rejected`, not `invalid_response` ([`provider.rs:1111-1116`](../../crates/arany/src/provider.rs)).
- Null usage carries no information about *where* the failure happened: every `CallResult::Failed` records `Usage::default()` and no response ID or provenance ([`run_loop.rs:1430-1437`](../../crates/arany/src/engine/run_loop.rs)). A complete stream that fails in post-stream decoding looks identical to one that failed at `message_start`.
- 1.2 s is consistent with a complete short Haiku response (one `tool_use` block) that then failed to decode; it is inconsistent with a timeout or a large output.

### `ProviderError::InvalidOutcome` sources on a tool-using Messages turn

Ranked by likelihood; all are hypotheses until the S1 stage log or a fixture reproduces one.

1. **Required-nullable Read fields omitted by the model.** `arany_read` is offered with `"required": ["path", "offset", "limit"]` and `type: ["integer","null"]` ([`crates/arany/src/provider/snapshots/anthropic_tools.json`](../../crates/arany/src/provider/snapshots/anthropic_tools.json), built by [`crates/arany/src/tools/catalog.rs:386-404`](../../crates/arany/src/tools/catalog.rs) from [`crates/arany/src/tools/fs.rs:79-93`](../../crates/arany/src/tools/fs.rs)). The typed proposal is deserialized with `#[serde(deserialize_with = "Option::deserialize")]` and no `default` ([`crates/arany/src/tools/types.rs:234-240`](../../crates/arany/src/tools/types.rs)), so `{"path": "AGENTS.md"}` fails with a missing-field error, which [`crates/arany/src/provider/decode.rs:124`](../../crates/arany/src/provider/decode.rs) maps to `InvalidOutcome`. Anthropic tools are sent non-strict ([`wire.rs:477`](../../crates/arany/src/provider/anthropic/wire.rs); [provider spec](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding)), so the API does not enforce `required`; `Search` already tolerates omission via `default` + `required_nullable` ([`types.rs:242-275`](../../crates/arany/src/tools/types.rs)), `Read` does not. The owner's own P1 probe lists "required-nullable field compliance" as unmeasured ([NEXT_STEPS.md](../../NEXT_STEPS.md#message-native-provider-request-owner-run-live-probes)). Every existing fixture sends `offset: null, limit: null` explicitly ([`wire/tests.rs:72-74`](../../crates/arany/src/provider/anthropic/wire/tests.rs), [`decode.rs:206-209`](../../crates/arany/src/provider/decode.rs)); no test covers omission. Fits a plain-text turn succeeding and the first Tool turn failing exactly.
2. **Unknown field on a content block of the final message.** `WireContent` is `deny_unknown_fields` ([`wire.rs:612-626`](../../crates/arany/src/provider/anthropic/wire.rs)) and the stream clones each `content_block_start` block verbatim ([`wire.rs:145`](../../crates/arany/src/provider/anthropic/wire.rs)), so any field Anthropic adds to a `tool_use` or `thinking` block (not present on `text` blocks, which Run A proved) rejects at [`wire.rs:363-364`](../../crates/arany/src/provider/anthropic/wire.rs) with `InvalidOutcome`. Claude Haiku 5.5 runs adaptive thinking by default (claude-api skill, thinking table), so a Tool turn may carry `thinking` blocks that a one-word `hola` turn did not. Unverified: the exact block fields current models emit; no web tool was used.
3. **Content-shape rejections in `tool_requests`** ([`wire.rs:797-822`](../../crates/arany/src/provider/anthropic/wire.rs)): two `tool_use` blocks despite `disable_parallel_tool_use` (`requests.len() != 1`), any block after the `tool_use` (`!requests.is_empty()`), a name outside the offered list, or arguments over [`argument_limit`](../../crates/arany/src/provider/decode.rs). Mixed and over-cap shapes are pinned offline by `inbound_rejects_max_tokens_refusal_mixed_and_over_cap` ([`wire/tests.rs:394`](../../crates/arany/src/provider/anthropic/wire/tests.rs)); whether Haiku emits them live is unknown.
4. **Admission rejections in `WireResponse::admit`** ([`wire.rs:735-754`](../../crates/arany/src/provider/anthropic/wire.rs)): a stop reason other than `end_turn`/`tool_use` (Haiku 5.5 can stop with `refusal`, which Arany fails closed by contract), non-null `stop_details`, more than 16 blocks, or output above the cap. Model equality and usage were already proven by Run A.
5. **Low likelihood, same class:** selected-key reflection in the bounded body or decoded outcome ([`anthropic.rs:222-224, 250-252`](../../crates/arany/src/provider/anthropic.rs); [`wire.rs:317-319`](../../crates/arany/src/provider/anthropic/wire.rs)), the opaque-turn byte bound ([`wire.rs:383-388`](../../crates/arany/src/provider/anthropic/wire.rs)), or `ToolUseId` validation ([`crates/arany/src/provider/conversation.rs:179-185`](../../crates/arany/src/provider/conversation.rs)).

Excluded by the record: every `InvalidStream` path in the event decoder (ordering, `tool_use` start with non-empty `input`, unknown delta types, `message_delta` usage, `MAX_CONTENT_BLOCKS` at stream time, `stopped` re-entry), `StreamEnded`, `InvalidResponseContract`, `InvalidOutcomeContract`, HTTP status failures and transport failures: each records a non-null reason or a different disposition.

### ChatGPT pattern to copy

The subscription decoder keeps a `stage: Stage` on the decoder ([`crates/arany/src/provider/openai/subscription.rs:721-760`](../../crates/arany/src/provider/openai/subscription.rs)), sets it before each check, and on error calls `diagnostics::subscription_failure(stage, counts)` ([`subscription.rs:585-615`](../../crates/arany/src/provider/openai/subscription.rs)), which writes `subscription stage=… bytes= events= http_status=` plus a bounded `ToolArgumentShape` in development builds only ([`crates/arany/src/diagnostics.rs:28-71, 91-160, 238-270`](../../crates/arany/src/diagnostics.rs)). The journal reason stays coarse; the fine stage lives in the local log. Existing closed stages already cover the decode half this plan needs (`ResponseEnvelope`, `ResponseModel`, `ResponseStatus`, `ResponseContent`, `OutcomeToolArguments`, `OutcomeReadBounds`, `UsageContract`, …).

### Journal versioning

`ProviderCallRecorded` is at payload version 19 ([execution spec, version table](../../docs/specs/execution.md#admission-persistence-and-recovery)). The spec's rule for a new enum variant carried by a payload ([execution spec](../../docs/specs/execution.md), "An additive optional field…") is one payload version step with an identity migration and canonical fixture in `store::migrate`, enforced by test ([AGENTS.md rule 5](../../AGENTS.md#global-rules)). Adding a closed reason to `ProviderFailureReason` therefore means 19 → 20.

### Test coverage today

Offline Messages corpora ([`wire/tests.rs`](../../crates/arany/src/provider/anthropic/wire/tests.rs)) cover: non-streamed `tool_use` with text and thinking before the call (216), provenance (299), mixed/over-cap/refusal rejections (394), streamed block closure and thinking exclusion (1208), streamed `input_json_delta` assembly and argument bounds (1288, 1441, 1538, 1567), a quoted preamble before `tool_use` (1899), and answer withdrawal when a call follows (2156). Not covered: a `tool_use` input omitting a required-nullable field, a content block carrying an unknown field, and two `tool_use` blocks in one stream. No integration corpus under `crates/arany/tests` exercises the Anthropic wire.

Peer behavior (from memory, unverified from source in this pass): Claude Code's `Read` declares `offset` and `limit` optional; Arany's required-nullable choice exists for OpenAI strict schemas, which the Messages route does not use.

## Contract and scenarios

- Owning spec: [Provider](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding) and [Execution](../../docs/specs/execution.md#accepted-outcomes) (closed failure reasons, version table); [Diagnostics](../../docs/specs/diagnostics.md) for `development.log`.
- Proposed change: (S1) development builds record `messages stage=<closed stage> bytes= events= http_status=` for every rejected Messages stream or decoded Messages response, with the bounded `ToolArgumentShape` when a typed proposal was reached; (S2) the `InvalidOutcome` error class records a closed reason instead of `null`; (S3) the confirmed cause is fixed by contract, with the fixture row that reproduces it.
- Successful path: Run B's prompt yields `tool_requested`, the Read step runs, and the Run continues to an `end_turn` answer.
- Rejection/failure: a response that still fails names its stage locally and its closed reason in the journal; replay of a journal written before S2 is unchanged by the identity migration; a Tool proposal that fails typed decoding after S3 either decodes (omitted nullable field) or is refused as `invalid_arguments` with the committed refusal result, never a silent `null`.
- Evidence owners: `wire/tests.rs` rows for each new shape; `diagnostics.rs` for the log line format; `store::migrate` fixture for 19 → 20; the owner's live re-run for the cause.

## Chosen direction

- Simplest sufficient solution after Ponytail: reuse `SubscriptionFailureStage`, `StreamCounts`, `ToolArgumentShape` and `write_failure` as they are, adding only a site label (`messages`) and the few stream-ordering variants Messages needs; keep a `stage` field on `MessageStream` and a stage return from `decode_run`, mirroring the subscription decoder. No new module, trait or configuration. The journal change is one enum variant plus the mandatory migration step.
- Linux/macOS: no OS impact; `development.log` path differences are already owned by [Diagnostics](../../docs/specs/diagnostics.md).
- Ownership: unchanged. The stage stays local to the adapter and the diagnostics writer; the Engine keeps mapping `ProviderError` to the journal reason.

## Security and resource model

The stage line and `ToolArgumentShape` carry closed enums, counts and lengths only: no response text, argument values, paths, model text or credentials, exactly as the existing subscription record. `BoundedRecord` bounds the line. No new network, file or credential access; no trust boundary changes. Writing the log is development-build only.

## Risks and mitigations

- The live cause is none of the ranked candidates → S1 still names the stage, so the next run is diagnosable; S3 is re-planned from that evidence.
- Cause 1 fixed by relaxing `Read` to optional-nullable changes the canonical serialized call → keep serialization unchanged (`null` written as today) and add the decode row to `native_tool_paths_normalize_only_peer_spelling_before_admission`; OpenAI strict schemas still require the fields on the wire.
- Payload version bump touches replay of existing journals → identity migration with fixture and the `store::migrate` adjacency test, as for Glob and MultiEdit.

## Vertical slices

1. **S1: Messages failure stages in `development.log`.** Input: any rejected Messages stream or decoded response → adapter sets the stage before each check (`MessageStream::event`, `read_streamed_reply`, `decode_run`, `WireResponse::admit`, `tool_requests`) → `diagnostics::messages_failure(stage, counts)` writes `messages stage=… bytes= events= http_status=` and the `ToolArgumentShape` when a typed proposal existed (reached via a `tool_request` that returns which shape failed, as the subscription decoder does). Tests: one table in `wire/tests.rs` asserting the stage per rejected fixture (including the three uncovered shapes above, which also serve as S3 candidates); one `diagnostics.rs` row for the log line. Spec: one sentence in [Diagnostics](../../docs/specs/diagnostics.md) listing the `messages` site next to `subscription`. One PR.
2. **S2: Closed journal reason for the `InvalidOutcome` class.** `ProviderError::InvalidOutcome` records a new closed `ProviderFailureReason` (proposed name `outcome_decode`: a framed response whose content did not decode into an offered answer or typed proposal; distinct from `outcome_contract`, which triggers the single corrective call, and from `response_contract`, which covers post-decode identity and usage). `ProviderCallRecorded` 19 → 20 with identity migration, fixture and adjacency test; [Execution](../../docs/specs/execution.md) version table and closed-reason list updated; `run_loop.rs` test row for the mapping. Independent of S1. One PR.
3. **S3: Fix the confirmed cause with its reproducing fixture.** Blocked on the owner's re-run below. If cause 1: decode `Read.offset`/`Read.limit` as nullable with omission allowed (same `default` + `required_nullable` idiom as `Search.include`), keep serialization canonical, add the omission row to the decode and Messages corpora, and update the [Provider spec](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding) sentence on required-nullable fields. If cause 2: admit the named field on the affected block type (never a blanket `deny_unknown_fields` removal) with a fixture copied from the official wire contract. If cause 3 or 4: no decoder change; record the shape in the spec as failing closed and move the live finding to P2. One PR.

## Owner live re-run (after S1 merges)

1. Same account, `claude-haiku-5-5`, effort `low`, new Session, development build (`cargo run`).
2. Prompt, verbatim: `Read AGENTS.md and agents/git.md, then explain in three sentences which git operations are forbidden and why.`
3. Inspect `arany show` for the first `ProviderCallRecorded`: expected `tool_requested` (fixed) or `invalid_response` with the S2 reason once S2 is in.
4. Inspect `development.log` (path per [Diagnostics](../../docs/specs/diagnostics.md)) for the first `messages stage=` line and, when present, the `tool_arguments ToolArgumentShape { operation, read_offset, read_limit, … }` line. `stage=OutcomeToolArguments` with `operation: "read"` and absent offset/limit confirms cause 1; a `ResponseContent` stage after a complete stream points at cause 2; `ResponseContent` with `response_shape` showing two tool blocks points at cause 3.
5. Report the stage line only (it contains no model text); never paste the request or response body.

## Exit criteria

- S1: each rejected Messages fixture in the corpus asserts its stage; a development run of Run B's prompt writes one `messages stage=` line; CI green.
- S2: `ProviderCallRecorded` 20 migrates a version-19 fixture byte-for-byte; `InvalidOutcome` records the new reason in the `run_loop.rs` table; CI green.
- S3: Run B's prompt completes live with `tool_requested` then an answer on `claude-haiku-5-5`/`low` (owner evidence, recorded in NEXT_STEPS P1); the reproducing fixture row is in `wire/tests.rs`; the spec sentence that owns the fixed behavior is updated.
- Closeout: accepted behavior moves into the Provider, Execution and Diagnostics specs; this plan is deleted under the [planning lifecycle](../README.md#lifecycle); unresolved live checks stay in NEXT_STEPS P1/P2.
