# Write and Edit content up to 64 KiB

## Metadata

- Status: `Draft`
- Owner: Arany maintainers
- Last updated: `2026-10-21`

## Outcome and scope

Write accepts up to 65,536 decoded UTF-8 bytes of content; Edit accepts up to 65,536 decoded UTF-8 bytes **combined across `old` and `new`**. This removes forced chunking for ordinary file edits while preserving journal-first mutation, digest preconditions and Guard publication. It delivers the Write/Edit portions of Daily use first items 5 and 10. MultiEdit, command changesets, Goal criteria, draft attachments and Goal defaults are separate work.

## Research

Verified against this worktree on 2026-10-21; citations identify the current baseline, not implemented changes.

| Finding | Source |
|---|---|
| Write content and combined Edit strings currently stop at 8 KiB. Edit also requires a nonempty unique old string and a digest. Its schema advertises 8192 bytes. | [crates/arany/src/tools/fs.rs:25](../../crates/arany/src/tools/fs.rs), [fs.rs:84](../../crates/arany/src/tools/fs.rs), [fs.rs:1278](../../crates/arany/src/tools/fs.rs); [Tools limits](../../docs/specs/tools.md#limits-and-failure-behavior) |
| Serialized arguments are 16 KiB; results are 64 KiB; observations are twice the result allowance plus the argument allowance. File input is 1 MiB; command changesets have their own 8 KiB decoded cap. | [crates/arany/src/tools/types.rs:13](../../crates/arany/src/tools/types.rs), [types.rs:18](../../crates/arany/src/tools/types.rs), [types.rs:24](../../crates/arany/src/tools/types.rs), [types.rs:38](../../crates/arany/src/tools/types.rs) |
| Shared structured decoding and both native stream adapters consume the argument bound; OpenAI diagnostic classification separately repeats the 8 KiB content limits. | [crates/arany/src/provider/decode.rs:69](../../crates/arany/src/provider/decode.rs), [decode.rs:90](../../crates/arany/src/provider/decode.rs), [provider/anthropic/wire.rs:189](../../crates/arany/src/provider/anthropic/wire.rs), [provider/openai/wire.rs:178](../../crates/arany/src/provider/openai/wire.rs), [wire.rs:268](../../crates/arany/src/provider/openai/wire.rs), [wire.rs:638](../../crates/arany/src/provider/openai/wire.rs) |
| Ordinary Event payloads, including ToolStarted, currently use 64 KiB. ToolFinished has a separate observation-plus-512-byte bound; HookFinished uses the observation bound. Append and replay consult the per-kind limit. | [crates/arany/src/store.rs:40](../../crates/arany/src/store.rs), [store.rs:46](../../crates/arany/src/store.rs), [store.rs:50](../../crates/arany/src/store.rs), [store/journal.rs:240](../../crates/arany/src/store/journal.rs), [store/replay.rs:899](../../crates/arany/src/store/replay.rs) |
| Admission currently reserves default Event bytes plus extra ToolFinished capacity. Enlarging ToolStarted therefore requires changing reservation accounting too. | [crates/arany/src/store/journal.rs:119](../../crates/arany/src/store/journal.rs) |
| Hook stdin is 32 KiB. PreToolUse rejects oversized full input. PostToolUse can truncate only its response; an oversized call still fails the empty-response preflight. Both failures deny before dispatch. | [crates/arany/src/tools/permissions/hooks.rs:16](../../crates/arany/src/tools/permissions/hooks.rs), [hooks.rs:409](../../crates/arany/src/tools/permissions/hooks.rs), [hooks.rs:444](../../crates/arany/src/tools/permissions/hooks.rs), [engine/run_loop/step/hooks.rs:295](../../crates/arany/src/engine/run_loop/step/hooks.rs), [hooks.rs:313](../../crates/arany/src/engine/run_loop/step/hooks.rs) |
| The Guard task frame is 96 KiB before its newline. Encoding rejects overflow; Linux embeds config and intent in that frame. | [crates/arany/src/tools/guard.rs:17](../../crates/arany/src/tools/guard.rs), [guard.rs:1271](../../crates/arany/src/tools/guard.rs), [guard/linux.rs:908](../../crates/arany/src/tools/guard/linux.rs) |
| Worker edits already consume the typed Write/Edit call and separately bound the resulting image file to 1 MiB. The other 8 KiB checks in worker/Goal criterion validation concern success criteria, not mutation strings. | [crates/arany/src/tools/worker.rs:1024](../../crates/arany/src/tools/worker.rs), [worker.rs:1082](../../crates/arany/src/tools/worker.rs), [session/worker.rs:265](../../crates/arany/src/session/worker.rs), [session/events/goal.rs:477](../../crates/arany/src/session/events/goal.rs) |

The supplied research found no documented peer content cap. [Claude Code's Edit/Write reference](https://code.claude.com/docs/en/tools-reference#write-tool-behavior) documents file operations without specifying a content ceiling. **Unverified:** actual Claude Code and Codex maximum content sizes; absence of a documented cap is not evidence of unlimited input. Arany retains explicit finite limits because durability and per-effect admission require them.

Editing and evidence rules: [planning workflow](../PLANNING.md), [library practice](../../agents/arany.md), [security pass](../../agents/security.md#security-pass-triggers) and [testing](../../agents/testing.md#user-visible-proof). The [system overview](../../docs/architecture/system-overview.md#2-runtime-architecture) keeps policy in Engine, canonical state in Journal/Store and file effects in Guard.

## Contract and scenarios

Owning spec: [Tools](../../docs/specs/tools.md#limits-and-failure-behavior), including hook input and Guard framing. Update the [Execution persistence table](../../docs/specs/execution.md#persistence-bounds) for the Event exceptions below. Provider request, response, token and context limits retain their [existing owner](../../docs/specs/provider.md); an admissible content size does not guarantee space in a filled Run.

| Scenario | Required observation and evidence owner |
|---|---|
| Unhooked Write at 64 KiB; Edit with combined strings at 64 KiB | Accept through native and structured decoding, commit the complete intent before GO, publish atomically and retain digest/checkpoint metadata. Extend [Tool boundary corpus](../../crates/arany/src/tools/tests.rs), [Anthropic wire corpus](../../crates/arany/src/provider/anthropic/wire/tests.rs), [OpenAI wire corpus](../../crates/arany/src/provider/openai/wire.rs) and [Guard journey](../../crates/arany-cli/tests/guard_runs.rs). |
| 64 KiB + 1 decoded byte; combined Edit overflow; multibyte content | Reject the whole proposal, counting bytes rather than characters; no partial file. Use the same boundary tables. |
| Escape-heavy content and long valid path at the decoded boundary | Actual serialized arguments, intent Event and task fit their new ceilings, including six-byte JSON escapes. A JSON frame exactly at its ceiling is accepted; one byte over is rejected before dispatch. Tool, Store and Guard framing corpora own these boundaries. |
| Matching PreToolUse or PostToolUse hook cannot receive the complete call within 32 KiB | Deny the Tool before any handler starts or file effect occurs; record the hook-origin permission denial with zero handlers. Never truncate `tool_input`, hash it in place of content, bypass the hook or retry unhooked. Extend [permission corpus](../../crates/arany/src/tools/permissions/tests.rs) and [Guard permission journeys](../../crates/arany-cli/tests/guard_runs/permissions.rs). |
| Stale digest, ambiguous old text, forbidden path, full Store or insufficient context | Preserve existing conflict/denial/capacity behavior before publication; no automatic limit relaxation. Guard journeys and [Store admission corpus](../../crates/arany/src/store/journal/tests.rs) own these failures. |
| Completion lost after GO; closed journal reopened after a successful large edit | Unknown outcome remains Uncertain and stops continuation. Strict replay renders the committed facts and never repeats the mutation or hooks. Extend [session Tool journeys](../../crates/arany-cli/tests/session_run/tools.rs) and the Store corpus. |

## Chosen direction

Ponytail: raise shared typed-content validation and the existing serialized transport bounds; retain the existing JSON path, concrete Store, native Guard adapters and changeset receipts. No attachment store, side-channel content transfer or new dependency is needed.

Proposed ceilings are exact binary units:

| Boundary | Proposed ceiling and overflow |
|---|---|
| Write content; Edit `old.len() + new.len()` | 64 KiB decoded UTF-8; reject whole above it. Keep a shared content constant so validation, descriptions and diagnostics agree. |
| `MAX_TOOL_ARGUMENT_BYTES` | 400 KiB (409,600 bytes), across all argument decoding and typed-call validation. Worst-case content escaping needs 6 × 64 KiB = 384 KiB; the remaining 16 KiB covers the bounded path, digest, keys and call envelope. Other operations retain their semantic field limits. Reject oversized serialized input before expensive parsing. |
| Ordinary `MAX_EVENT_BYTES` | Keep 64 KiB. |
| ToolStarted payload exception | 512 KiB (524,288 bytes), including the complete intent and its bounded metadata; use the same per-kind rule for append and replay. Reject overflow. |
| Observation / HookFinished payload | 528 KiB (540,672 bytes), from 2 × 64 KiB + 400 KiB. |
| ToolFinished payload | 528 KiB + 512 bytes (541,184 bytes), retaining checkpoint metadata allowance. |
| Guard base `TASK_BYTES_CAP` | 1 MiB (1,048,576 bytes), before the delimiter; reject whole before Ready. Keep the separately bounded definition extension and worker bulk-transfer protocol. |
| Hook stdin / Tool result | Keep 32 KiB / 64 KiB. A matched hook's complete input over 32 KiB denies the Tool in preflight, even when content itself is valid. |

Journal/Store owns payload acceptance and capacity reservation. Reserve extra bytes for every possible enlarged ToolStarted and HookFinished slot as well as ToolFinished, including hook-narrowed intents and worker paths; do not reserve only the old default per slot. Test exact serialized envelopes and near-full admission, rather than relying on the content calculation alone. These are bounds changes without a stored JSON shape change; keep current payload versions. Any implementation requiring a format change must include its migration and fixture under the [format rule](../../AGENTS.md#global-rules).

Engine still owns intent ordering, authorization and cancellation; Tools owns byte validation; Provider adapters translate external arguments at their edge; Guard owns atomic effect/publication. The 1 MiB edit-file bound, command changeset cap and Goal criterion caps stay separate. Linux and macOS share the bounds; update enforcement digests that pin the Guard cap and exercise both existing native adapters. No new OS facility is required; no native support claim follows from this plan.

## Security and resource model

Triggered pass: larger filesystem mutations and input bodies. Assets are Workspace content, checkpoint bytes, private journal capacity and model-disclosed context. The model controls content and arguments; repository files and hook output remain untrusted data. Authenticated settings and the pinned typed grant supply authority before reading or dispatch. Preserve immutable normalized intent, digest preconditions, no-follow path admission, protected names, child grant intersection and Guard GO/receipt checks. No new credential, network, startup-discovery or descendant privilege is introduced.

Reject decoded, serialized, Event or task overflow whole. Keep Store aggregate quotas, context/request accounting and Goal usage finite under their existing owners; larger arguments consume more of them. Keep hook output and post-response truncation distinct from forbidden input truncation. Reserve full durable headroom before effects; disk failure after dispatch can still leave Uncertain. Errors and diagnostics must not echo content or secret canaries. Bounds and synthetic evidence do not prove isolation from a hostile same-user/root process or live model completion.

## Risks and mitigations

- JSON escaping or duplicated metadata bypasses a guessed allowance → serialize worst-case calls and complete envelopes at every boundary, including native streaming and private-worker paths.
- Enlarged Events exhaust storage sooner → account for every enlarged slot before dispatch; keep quotas and capacity failures observable.
- A large hooked edit appears inexplicably unavailable → document the 32 KiB complete-input denial explicitly; preserve the hook restriction in committed output.
- A blanket replacement of `8 * 1024` changes unrelated behavior → use the shared content constant only for Write/Edit; leave criteria and command changesets intact.

## Vertical slices

1. **One PR: complete a 64 KiB Write or Edit durably through the existing Tool path.** Carry the new shared content bound through schemas, both native adapters and structured decoding → Engine admission and hooks → enlarged ToolStarted reservation/commit → Guard frame and atomic Workspace or private-worker mutation → receipt/checkpoint and ToolFinished → model result, `show` and closed-store replay. Include the Tools/Execution contract changes, exact-boundary/escape/denial/capacity/uncertainty scenarios above and the existing catalog snapshots. This single slice leaves large edits usable on its own; a constants-only intermediate PR would not.

## Exit criteria

- The owners above prove exact decoded and serialized boundaries, full-content persistence, hook denial before effects, stale-precondition refusal, headroom rejection and replay without execution.
- Write and Edit work through primary and existing delegated mutation paths without enlarging command changesets or Goal criteria. Ordinary Event and hook bounds remain as selected above.
- Run formatting, Clippy and targeted owning tests when implementing; complete final-head Linux/macOS CI before implementation handoff. Live Provider and native checks unavailable to the implementer remain owner-run in [NEXT_STEPS](../../NEXT_STEPS.md), without claiming evidence from this docs-only plan.
- Publish accepted behavior only in Tools and Execution, then remove this plan and its index/priority links under the [planning lifecycle](../README.md#lifecycle).
