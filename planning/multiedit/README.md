# MultiEdit: atomic ordered edits to one file

## Metadata

- Status: `Ready` (Research and Plan gates passed; implementation not started).
- Owner: Arany repository owner; implementation by the assigned Tools/Engine maintainer.
- Last updated: `2026-10-22`.

## Outcome and scope

Deliver one capability: apply an ordered list of literal replacements to one existing UTF-8 file through one admitted Tool operation, publishing all replacements or none.

This serves Arany’s coding loop while preserving journal-first execution, Guard isolation, digest checks, receipts, checkpoints and non-executing replay.

[Daily use first item 10](../../NEXT_STEPS.md) also contains separate features. Subsequent plans must cover typed delete, typed move, and parallel reads; parallel reads depend on Pursue item 10’s multi-call steps and probe P2. Keep the existing owner-run 64 KiB Write/Edit checks outstanding.

## Research

- **Current Edit:** requires a Workspace-relative path, expected SHA-256 and nonempty unique literal match. It computes replacement content, checks the 1 MiB file ceiling, then rechecks and publishes through a fresh inode and rename. Directory-sync failure after publication becomes `Uncertain`. [File implementation](../../crates/arany/src/tools/fs.rs), `valid`, Edit execution and `replace_file`.
- **Existing limits:** Write content and combined Edit strings are bounded at 64 KiB decoded UTF-8; their raw serialized arguments have a 400 KiB ceiling. Other operations ordinarily have 16 KiB. Command changesets have a separate 8 KiB aggregate content ceiling and explicitly lack multi-file transactional atomicity. [Tools contract](../../docs/specs/tools.md#limits-and-failure-behavior).
- **Existing integration:** private workers predict Edit results separately; checkpoints, rewind, Goal criteria, permissions and reports recognize Edit explicitly. These paths must recognize MultiEdit together. [Worker mutations](../../crates/arany/src/tools/worker.rs), [checkpoint ownership](../../crates/arany/src/session/checkpoint.rs).
- **Persistence:** adding a Tool enum variant requires payload-version advances and migration steps for its enclosing Event kinds; an unchanged SQLite table layout does not exempt it. Glob supplies the existing precedent. [Execution contract](../../docs/specs/execution.md), [migration registry](../../crates/arany/src/store/migrate.rs).
- **Claude Code, `1.0.88` and `1.0.128`:** official npm code exposes `MultiEdit {file_path, edits:[{old_string,new_string,replace_all?}]}`, with `replace_all` defaulting to false. It validates each edit against the original file, then transforms in array order in memory, rejecting a later search contained in an earlier replacement. Its advertised sequential semantics therefore do not allow arbitrary dependent edits. It prepares the whole result before one write-helper call; that helper attempts rename but falls back to direct writing, so strict filesystem atomicity is not established. The code also permits creation through an empty search. [Shipped-code evidence, locations and errors](./claude-code-evidence.md).
- **Claude removal:** `1.0.128` still registers MultiEdit; `2.0.0` removes it. Edit retains a single-replacement schema and an internal batch transformer; this does not establish a public batch interface folded into Edit. [Adjacent npm releases](./claude-code-evidence.md).
- **Codex CLI, `rust-v0.114.0`:** `apply_patch` accepts freeform patch text or `{input}`, supporting add/update/delete/move. It prepares changes for approval. Within one update, chunks match original lines and the complete result is computed before writing; across file hunks, writes/removals are sequential without rollback. This is neither Claude's replacement-array wire format nor an atomic multi-file primitive. [Handler, lines 113–137](https://github.com/openai/codex/blob/rust-v0.114.0/codex-rs/core/src/tools/handlers/apply_patch.rs#L113-L137), [applier, lines 260–467](https://github.com/openai/codex/blob/rust-v0.114.0/codex-rs/apply-patch/src/lib.rs#L260-L467).
- **OpenCode, `v1.1.13`:** `multiedit` accepts `filePath` and `edits[{filePath,oldString,newString,replaceAll?}]`; the wrapper ignores nested paths and awaits individual Edit executions in order. Each Edit rereads and writes the file, so later edits see earlier results and a late failure leaves earlier writes. Edit uses the `edit` permission and rejects identical strings; empty searches can write a file. [MultiEdit, lines 6–42](https://github.com/anomalyco/opencode/blob/v1.1.13/packages/opencode/src/tool/multiedit.ts#L6-L42), [Edit, lines 27–104](https://github.com/anomalyco/opencode/blob/v1.1.13/packages/opencode/src/tool/edit.ts#L27-L104).

## Decisions

- **Decided 2026-10-22 (owner, Q2):** lift the new-typed-Tool freeze for MultiEdit only in [item 10 and Frozen areas](../../NEXT_STEPS.md#frozen-areas). Typed delete, typed move, parallel reads and the existing live checks keep their scope and gates.
- **Decided 2026-10-22 (owner, Q3):** existing files only, ordered replacements against each preceding result, and at most 64 edits. Sequential dependency handling follows [OpenCode](https://github.com/anomalyco/opencode/blob/v1.1.13/packages/opencode/src/tool/multiedit.ts#L21-L35); it deliberately does not copy Claude's original-file validation and cross-edit substring rejection. This is the owner's accepted choice where peers differ, not a shared-peer claim. Creation stays with Write; the pinned existing-file digest and admitted path remain intact.
- **Decided 2026-10-22 (peer parity):** use Claude's replacement field names, false `replace_all` default, identical-string and unchanged-final-content rejection, and match-count checks; use the Edit permission family as both [Claude](./claude-code-evidence.md) and [OpenCode](https://github.com/anomalyco/opencode/blob/v1.1.13/packages/opencode/src/tool/edit.ts#L27-L104) do. Codex has a different patch protocol, so there is no shared wire format to copy.
- **Decided 2026-10-22 (guarantee constraint):** prepare the complete change and publish once through the existing Guard publisher. Whole-batch validation before writing follows Claude; Arany diverges from OpenCode's partial batch writes and Claude/Codex's direct-write paths only to retain [per-effect isolation and honest uncertainty](../../AGENTS.md#non-negotiable-guarantees): validate the admitted single-file changeset before publication, bind one receipt, and never convert an unknown publication into a retryable failure. No new multi-file transaction or rollback mechanism is introduced.
- **Decided 2026-10-22 (guarantee constraint):** retain exact literal matching, Workspace-relative paths, required digest, strict fields and bounded content from [Arany's typed-file contract](../../docs/specs/tools.md#authority-and-effect-admission). Peer fuzzy matching, arbitrary paths and implicit creation cannot alter the pinned file precondition or select a different effect. Keep the existing byte ceilings; the owner's 64-edit ceiling bounds transformation work under [durable, budgeted Goals](../../AGENTS.md#non-negotiable-guarantees).
- **Decided 2026-10-22 (owner standing rule):** no saved-default preservation design or compatibility reader. Only the existing mandatory enum payload-version step and fixture apply; [Execution](../../docs/specs/execution.md#admission-persistence-and-recovery) remains the owner.

## Contract and scenarios

Owning contract: [Tools](../../docs/specs/tools.md), covering schema, semantics, permissions, bounds and evidence owners. [Execution](../../docs/specs/execution.md) owns payload versions and migration behavior.

Tool name: `multiedit`; presentation name: `MultiEdit`. Use the verified Claude payload names, adding Arany’s digest requirement:

```json
{
  "file_path": "src/example.rs",
  "expected_digest": "<64 lowercase hexadecimal SHA-256 characters>",
  "edits": [
    {
      "old_string": "original",
      "new_string": "replacement",
      "replace_all": false
    }
  ]
}
```

- One existing regular UTF-8 file; `file_path` remains Workspace-relative. Absolute paths and creation through an empty search string are unavailable under the existing Edit admission contract.
- Evaluate edits in array order against the preceding edit’s result. Do not reorder, deduplicate or search every edit against the original file.
- `replace_all` defaults to false. False requires exactly one match; true replaces every non-overlapping match and requires at least one. Empty searches and identical old/new strings reject. Reject a batch whose final content equals the original, following Claude’s final-result check. Literal replacement removes only the matched bytes, with no implicit newline removal or quote normalization.
- Reject unknown fields. Provider adapters preserve semantic defaults while satisfying their existing strict-schema requirements.
- Validate every replacement before publication. A missing/ambiguous match, stale digest, invalid UTF-8, denied path or exceeded bound publishes nothing.
- A dependency such as `A → B`, then `B → C` succeeds. A valid first replacement followed by an invalid second replacement leaves the original file unchanged.
- Atomicity covers file contents: readers see the original or complete final version, never a prefix of edits. It does not promise knowledge of completion after a lost receipt.
- Cancellation before publication changes nothing. Interruption during publication or failed durability confirmation records unknown outcome and stops continuation without automatic retry or rollback.
- Success retains Edit’s before/after digest, length, executable-bit and checkpoint facts; presentation shows one Tool outcome. Private-worker success remains private until ordinary handover integration.

## Chosen direction

Apply Ponytail by extending the existing closed Tool enum, catalog, file family and publisher. Use standard string operations with checked sizing; add no crate, registry framework, patch language or daemon.

Tools owns validation and ordered transformation; Engine owns admission and execution order; the reducer owns durable state; Guard owns filesystem validation/publication; Store owns checkpoint bytes. Reuse one transformation implementation for Workspace execution and private-worker prediction.

Inside Guard, derive and validate one complete single-file change, then call the existing publisher once. Do not implement MultiEdit as repeated Edit dispatches or route it through the 8 KiB command changeset format. Worker results continue through acknowledged overlays, sealed handovers and validated changeset integration with receipts.

Linux and macOS share the transformation and existing typed-file Guard paths. Add no native API or stronger durability claim. Preserve fail-closed behavior when enforcement is unavailable; native filesystem and crash evidence remains platform-specific.

## Journal and Event impact

One batch is one immutable intent and one Tool action, with existing hook/approval Events, checkpoint capture and terminal observation—not one journaled effect per replacement.

Reserve Event, Store and Goal headroom before dispatch; commit `ToolStarted`, hook outcomes and permission verdict before Guard GO. Commit the attested `ToolFinished` before continuation. Validate receipt correlation and publication metadata in replay; never recompute edits from the current filesystem.

Add `ToolCall::MultiEdit` and the corresponding private-worker Tool label. Advance payload versions for `ToolStarted`, `ToolFinished`, `HookStarted`, `HookFinished`, `GoalPlanContractRecorded`, `ProviderCallRecorded`, `RunStarted` and `AgentSpawned`.

Follow the existing [payload-version and migration contract](../../docs/specs/execution.md#admission-persistence-and-recovery) for the new enum variant, with its required step and fixture; add no compatibility reader or saved-default preservation design. SQLite and checkpoint-store schemas need no structural change.

Update Goal scope/success matching, worker mutation validation, checkpoint/rewind attribution, Supervisor edit facts and reporting.

## Security and resource model

The new Tool/filesystem path triggers the [security pass](../../agents/security.md#security-pass-triggers).

Assets are Workspace integrity, private worker isolation, journal/checkpoint confidentiality and bounded execution. Model paths, strings and repository bytes are hostile data. Authority remains the admitted directory capability, pinned typed grant and user-owned restrictions; startup trust ordering is unchanged.

- Map MultiEdit to the `Edit` permission family, including path rules, deny-before-ask-before-allow, read-denial protection, Goal scope and `acceptEdits`. Approval cannot widen the grant.
- Hooks remain restrict-only. Show `MultiEdit` with the complete bounded batch; reject unsupported input rewrites rather than treating them as authority.
- Researchers cannot invoke it. Editors require an explicit child capability within the parent grant and existing lease.
- Preserve protected-name, no-follow, hard-link, mount, file-type and directory-identity checks through publication.
- No shell, network, credentials, new descendants or host fallback. Diagnostics expose bounded classifications, not replacement bodies; model output excludes private checkpoint content.

| Resource | Bound and overflow |
|---|---|
| Files / operations | Exactly one file; 1–64 edits; reject empty or excess arrays |
| Replacement content | Sum of all decoded `old_string` and `new_string` bytes ≤64 KiB |
| Arguments | Extend the existing 400 KiB raw and compact serialized ceiling to MultiEdit |
| File buffers | Original, every intermediate and final version ≤1 MiB; check arithmetic before allocation |
| Work | At most 64 bounded transformation passes; no recursive replacement or retry |
| Results / storage | Existing 64 KiB result ceiling; existing observation, catalog, checkpoint, worker and Goal bounds remain |
| Runtime / concurrency | Existing pinned file-operation deadlines and Guard admission; edits execute sequentially |

## Risks and mitigations

- Partial publication from looping over Edit → transform privately and publish once.
- Replacement expansion exceeding memory bounds → preflight each result with checked arithmetic.
- Permission, checkpoint or worker bypass through an omitted match arm → exhaustively update consumers and extend existing interface corpora.
- False rollback claims after rename → retain `Uncertain`, stop continuation and require explicit inspection before another attempt.
- Peer drift or undocumented behavior → retain versioned citations and distinguish actual code from tool descriptions and Arany guarantees.

## Vertical slices

**One vertical slice, one PR: `feat(tools): add atomic single-file MultiEdit`.**

The smallest usable increment crosses Provider schemas/decoding → catalog and grants → Engine policy/hooks/approval → journal/migrations → Workspace and private-worker Guard execution → receipts/checkpoints/replay → model and terminal presentation.

Ship all contract and evidence changes in that PR. A types-only, parser-only or journal-only PR would not deliver an independently usable capability.

Extend existing evidence owners:

- `tools/tests.rs` and worker corpus: ordering, dependent edits, replace-all, unchanged-final-content rejection, late failure without publication, UTF-8/CRLF fidelity, executable-bit preservation and exact boundary/overflow cases.
- Provider corpora: advertised schema, omitted defaults, escaped 64 KiB content, oversized raw arguments and unavailable-tool refusal across supported routes.
- Permission/Engine corpora: Edit allow/ask/deny, read-denial protection, denied hooks, grant narrowing, researcher rejection and private-worker containment.
- Reducer/migration corpora: malformed receipts, current payload round-trips, required enum-version migration fixture, grant narrowing and non-executing replay.
- `guard_runs` and CLI observation owners: real filesystem publication, cancellation/crash stages, checkpoint/rewind, unknown outcomes, exact output and reopened SQLite facts.

## Phase gates

**Research → Plan passed:** shipped Claude Code, pinned Codex/OpenCode and the current Arany flow are verified above; no owner-only question remains.

**Plan → Implement passed:** Q2 and Q3 are accepted, the contract and single vertical slice are ready. This documentation update does not start implementation.

**Implement next:** land the single vertical slice; stop and revise the research/plan if implementation contradicts its premises.

## Exit criteria

All successful, rejection, failure and recovery observations above pass through their named owners. The Tools and Execution contracts describe the shipped behavior and migration.

During implementation, run formatting, Clippy and targeted tests per [development verification](../../docs/development.md#verification); require complete final-head Linux/macOS CI before handoff. Build only in this worktree’s default `./target` or through the approved cargo wrapper; never set `CARGO_TARGET_DIR`.

Record unavailable native and live Provider evidence in `NEXT_STEPS.md`, preserving the existing 64 KiB Write/Edit checks. Synthetic tests establish neither live compatibility nor native support.

On completion, close the plan under `planning/README.md`, update inbound links and remove only MultiEdit from item 10; retain delete, move, parallel reads and unresolved evidence.
