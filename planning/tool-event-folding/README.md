# Tool-step Event folding and lenient inspection

## Metadata

- Status: `In progress` — next: slice 2, Engine pre-validation through the reducer (after G5).
- Owner: harness maintainers
- Last updated: 2026-10-25

## Outcome and scope

Remaining work: (a) the Engine runs the reducer's `ToolStarted` validation before dispatching a step, so a would-be `InvalidHistory` on resume becomes a typed refusal with a reason at proposal time; (b) `arany show` falls back to a lenient per-Event projection with a visible notice when strict replay rejects a history, never for resume or continuation.

Responsibility served: journal-first durability at lower cost ([priority 1](../../AGENTS.md#product-and-implementation-priorities)); global rule 5 (one current format, migration with fixture, archive below the floor). Provider reservations and hook Events retain the [execution contract](../../docs/specs/execution.md#committed-event-stream).

## Research

- The current folded-start, conditional proposal and archive contracts live in [execution](../../docs/specs/execution.md#committed-event-stream) and [Tools](../../docs/specs/tools.md#permission-modes-rules-and-hooks); they are prerequisites for the remaining slices.
- Validation on append already runs the reducer: `Journal::append` validates each transition and the four `begin_write` callers reuse the cached admission ([overview §2](../../docs/architecture/system-overview.md#2-runtime-architecture); the gitignored `.lavish/architecture-maps-after.html` review, problem 3). The live/replay divergence the review found is in Engine decisions taken without an Event (editor delegation, G5). For Tool steps, `check_tool` ([run_loop.rs:1287](../../crates/arany/src/engine/run_loop.rs)) and `primary_lease_allows` ([step/permission.rs:364](../../crates/arany/src/engine/run_loop/step/permission.rs)) decide before the intent commits; the reducer's `ToolStarted` validation (`session/events/validate/tool.rs`, `reducer/tool/lifecycle.rs`) is a second implementation of overlapping predicates.
- `show` uses strict read-only replay ([overview §4](../../docs/architecture/system-overview.md#4-public-command-contract)); a rejected history yields a typed State/replay error and no transcript ([goals.md Current-format replay](../../docs/specs/goals.md#current-format-replay-and-exec)).
- Peers: Codex writes one `ResponseItem` per function call and output plus a few turn-level Events; its persistence policy skips every Begin/Delta/approval-request event as "transient, non-durable" ([rollout/src/policy.rs](https://github.com/openai/codex/blob/main/codex-rs/rollout/src/policy.rs)); `reconstruct_rollout` tolerates unknown or partial items and never re-executes ([rollout_reconstruction.rs](https://github.com/openai/codex/blob/main/codex-rs/core/src/session/rollout_reconstruction.rs)). Claude Code records a message, a tool use and a tool result per call in a JSONL whose format "is internal to Claude Code and changes between versions" ([sessions](https://code.claude.com/docs/en/sessions)); interrupted tool calls are marked cut-off, not re-run. Goose stores messages in SQLite with a 17-step migration chain ([session_manager.rs](https://github.com/block/goose/blob/main/crates/goose/src/session/session_manager.rs)).

## Contract and scenarios

- Owning specs: [execution.md Committed Event stream](../../docs/specs/execution.md#committed-event-stream), [Admission, persistence and recovery](../../docs/specs/execution.md#admission-persistence-and-recovery) (payload versions, Event order), [Journal migration](../../docs/specs/execution.md#journal-migration) (floors, archive), [tools.md Permission modes, rules and hooks](../../docs/specs/tools.md#permission-modes-rules-and-hooks) (where the verdict is recorded), [goals.md Current-format replay and exec](../../docs/specs/goals.md#current-format-replay-and-exec) and [terminal.md](../../docs/specs/terminal.md) (`show` output), [serve.md](../../docs/specs/serve.md)/[acp.md](../../docs/specs/acp.md) (Event kinds exposed on the wire).
- Shared-validator contract: before committing the folded `ToolStarted`, the Engine calls the reducer's `ToolStarted` validator on the candidate against the current committed view; a rejection becomes a `ProposalRefusalCause` with a bounded reason, never an append attempt. The validator is exported narrowly at the Session boundary (mirroring G5's `validate_request`). Replay still rejects the same forged histories.
- Read-only inspection contract: `arany show` on a history that strict replay rejects prints the committed prefix projection up to the rejecting sequence, then a notice `History invalid at sequence N: <typed cause>; shown read-only, not resumable`, then a per-Event lenient listing (kind, sequence, decoded payload if it decodes, otherwise `undecodable`) for the remainder; exit status is nonzero; resume, fork, `exec --session` and compaction keep refusing. JSONL `show` adds `{"notice": ...}` lines with the same facts.
- Scenarios: the Engine refuses a step the validator would reject with the same cause; `show` on an `InvalidHistory` store prints prefix, notice and lenient tail; valid-history `show` stays byte-identical.
- Evidence owners: `session/reducer/tests.rs` and `crates/arany/tests/engine_run/permissions.rs` (validator/refusal parity), `crates/arany-cli/tests/session_run/tools/replay.rs` and a lenient case beside `rewind.rs` (strict continuation and read-only inspection).

## Chosen direction

- Simplest sufficient solution: export the existing validator through the Session boundary and add one read-only fallback branch in `show`; reuse the current Event vocabulary and pending Tool projection.
- OS impact: none.
- Interface change: `session` exports `validate_tool_started(&SessionView, &ToolStarted) -> Result<(), ToolRefusal>`; the [execution contract](../../docs/specs/execution.md) owns refusal causes.

## Security and resource model

No trust input changes. The verdict's authority is unchanged (same `PermissionVerdict`, same basis/source provenance), only its container. Lenient `show` renders undecodable payloads as a fixed marker, never raw bytes, and keeps the existing control-character sanitization; it grants nothing (no resume). Archive-below-floor is the existing rule-5 path with its existing notice and bounds.

## Risks and mitigations

- Lenient `show` tempts callers into treating invalid histories as usable → nonzero exit, explicit notice, and resume/fork/exec unchanged; pinned.

## Vertical slices

2. **Engine pre-validates through the reducer.** Export `validate_tool_started`; call it in `prepare_step_intent` before the append; map rejection to a typed refusal with reason; delete the duplicated predicates in `check_tool`/`primary_lease_allows` that the validator now owns (keep Engine-only liveness and budget gates). Specs: `execution.md` accepted outcomes (refusal causes), `tools.md` authority section (one validator). ≤500.
3. **Lenient `show`.** Fallback projection in the `show` path (`crates/arany-cli/src/main.rs::show`, `cli/record_formats.rs`), notice line and JSONL line, nonzero exit; replay entry returns the rejecting sequence and cause for the prefix (already typed). Specs: `goals.md` Current-format replay and `exec`, `terminal.md` `show` section. ≤400.

## Exit criteria

- Remaining scripted evidence: Engine refusal parity with the validator, lenient `show` on an invalid store with nonzero exit; `show` byte-identical for valid stores before and after.
- Contract updates listed per slice; no `NEXT_STEPS` item is replaced by this plan (none exists for it). Delete this folder when slice 3 lands.

## Gates

Slice 2: after G5 (shares the Session-boundary validator export pattern). Slice 3: no code gate; `main.rs`/`record_formats.rs` are not in any follow-up. Coordinate with [multi-call-steps](../multi-call-steps/README.md) slice 1: whichever lands second rebases on the other's `ToolStarted` shape.
