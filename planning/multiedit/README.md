# MultiEdit: atomic ordered edits to one file

## Metadata

- Status: `Draft`; Research gate remains open for the Claude contract gap below.
- Owner: Arany repository owner; implementation by the assigned Tools/Engine maintainer.
- Last updated: `2026-10-21`.
- Intended location: `planning/multiedit/README.md`; delivered here only. No files edited, builds or tests run.

## Outcome and scope

Deliver one capability: apply an ordered list of literal replacements to one existing UTF-8 file through one admitted Tool operation, publishing all replacements or none.

This serves Arany’s coding loop while preserving journal-first execution, Guard isolation, digest checks, receipts, checkpoints and non-executing replay.

[Daily use first item 10](/home/fpmirabile/projects/harness-team/wt/plan10/NEXT_STEPS.md:26) also contains separate features. Subsequent plans must cover typed delete, typed move, and parallel reads; parallel reads depend on Pursue item 10’s multi-call steps and probe P2. Keep the existing owner-run 64 KiB Write/Edit checks outstanding.

## Research

- **Current Edit:** requires a Workspace-relative path, expected SHA-256 and nonempty unique literal match. It computes replacement content, checks the 1 MiB file ceiling, then rechecks and publishes through a fresh inode and rename. Directory-sync failure after publication becomes `Uncertain`. [Validation and execution](/home/fpmirabile/projects/harness-team/wt/plan10/crates/arany/src/tools/fs.rs:14), [publisher](/home/fpmirabile/projects/harness-team/wt/plan10/crates/arany/src/tools/fs.rs:1643).
- **Existing limits:** Write content and combined Edit strings are bounded at 64 KiB decoded UTF-8; their raw serialized arguments have a 400 KiB ceiling. Other operations ordinarily have 16 KiB. Command changesets have a separate 8 KiB aggregate content ceiling and explicitly lack multi-file transactional atomicity. [Tools contract](/home/fpmirabile/projects/harness-team/wt/plan10/docs/specs/tools.md:603).
- **Existing integration:** private workers predict Edit results separately; checkpoints, rewind, Goal criteria, permissions and reports recognize Edit explicitly. These paths must recognize MultiEdit together. [Worker mutations](/home/fpmirabile/projects/harness-team/wt/plan10/crates/arany/src/tools/worker.rs:1040), [checkpoint ownership](/home/fpmirabile/projects/harness-team/wt/plan10/crates/arany/src/session/checkpoint.rs:70).
- **Persistence:** adding a Tool enum variant requires payload-version advances and migration steps for its enclosing Event kinds; an unchanged SQLite table layout does not exempt it. Glob supplies the existing precedent. [Execution contract](/home/fpmirabile/projects/harness-team/wt/plan10/docs/specs/execution.md:179), [migration registry](/home/fpmirabile/projects/harness-team/wt/plan10/crates/arany/src/store/migrate.rs:64).
- **Claude Code, legacy:** a CLI 1.0.88 transcript shows `MultiEdit {file_path, edits:[{old_string,new_string}]}`. Another report demonstrates `replace_all`, identical-string rejection and interactions between successive replacements. These are firsthand observations, not normative guarantees. [Versioned transcript](https://github.com/anthropics/claude-code/issues/6335), [failure report](https://github.com/anthropics/claude-code/issues/7197).
- **Claude evidence gap:** the [current official reference](https://code.claude.com/docs/en/tools-reference) omits MultiEdit. Historical ordering, complete failure semantics and atomicity were not verified from retrievable primary documentation/source. Do not claim those implementation properties as established.
- **Codex CLI, `rust-v0.114.0`:** `apply_patch` accepts freeform patch text or `{input}`, supporting add/update/delete/move. It prepares changes for approval, but applies file writes/removals sequentially without rollback on later failure. It is not a transactional batch primitive. [Handler](https://raw.githubusercontent.com/openai/codex/rust-v0.114.0/codex-rs/core/src/tools/handlers/apply_patch.rs), [applier](https://raw.githubusercontent.com/openai/codex/rust-v0.114.0/codex-rs/apply-patch/src/lib.rs).
- **OpenCode, `v1.1.13`:** `multiedit` accepts `filePath` and `edits[{filePath,oldString,newString,replaceAll?}]`; the wrapper ignores nested paths and awaits individual Edit executions in order, without rollback. Its [edit documentation](https://opencode.ai/docs/tools/#edit) describes literal replacements and shared `edit` permission. [Pinned MultiEdit source](https://raw.githubusercontent.com/anomalyco/opencode/v1.1.13/packages/opencode/src/tool/multiedit.ts).

## Contract and scenarios

Owning contract: `docs/specs/tools.md`, covering schema, semantics, permissions, bounds and evidence owners. `docs/specs/execution.md` owns payload versions and migration behavior.

Proposed Tool name: `multiedit`; presentation name: `MultiEdit`. Copy the observed Claude payload names, adding Arany’s digest requirement:

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
- `replace_all` defaults to false. False requires exactly one match; true replaces every non-overlapping match and requires at least one. Empty searches and identical old/new strings reject.
- Reject unknown fields. Provider adapters preserve semantic defaults while satisfying their existing strict-schema requirements.
- Validate every replacement before publication. A missing/ambiguous match, stale digest, invalid UTF-8, denied path or exceeded bound publishes nothing.
- A dependency such as `A → B`, then `B → C` succeeds. A valid first replacement followed by an invalid second replacement leaves the original file unchanged.
- Atomicity covers file contents: readers see the original or complete final version, never a prefix of edits. It does not promise knowledge of completion after a lost receipt.
- Cancellation before publication changes nothing. Interruption during publication or failed durability confirmation records unknown outcome and stops continuation without automatic retry or rollback.
- Success retains Edit’s before/after digest, length, executable-bit and checkpoint facts; presentation shows one Tool outcome. Private-worker success remains private until ordinary handover integration.

## Chosen direction

Apply Ponytail by extending the existing closed Tool enum, catalog, file family and publisher. Use standard string operations with checked sizing; add no crate, registry framework, patch language or daemon.

The local Ponytail installation is absent; this research read the [repository-pinned revision](https://raw.githubusercontent.com/DietrichGebert/ponytail/e3ba2aa6f1e6f0bc4d69eb09c9f0d0a93af56156/skills/ponytail/SKILL.md). Restore the prescribed installation before implementation.

Tools owns validation and ordered transformation; Engine owns admission and execution order; the reducer owns durable state; Guard owns filesystem validation/publication; Store owns checkpoint bytes. Reuse one transformation implementation for Workspace execution and private-worker prediction.

Inside Guard, derive and validate one complete single-file change, then call the existing publisher once. Do not implement MultiEdit as repeated Edit dispatches or route it through the 8 KiB command changeset format. Worker results continue through acknowledged overlays, sealed handovers and validated changeset integration with receipts.

Linux and macOS share the transformation and existing typed-file Guard paths. Add no native API or stronger durability claim. Preserve fail-closed behavior when enforcement is unavailable; native filesystem and crash evidence remains platform-specific.

## Journal and Event impact

One batch is one immutable intent and one Tool action, with existing hook/approval Events, checkpoint capture and terminal observation—not one journaled effect per replacement.

Reserve Event, Store and Goal headroom before dispatch; commit `ToolStarted`, hook outcomes and permission verdict before Guard GO. Commit the attested `ToolFinished` before continuation. Validate receipt correlation and publication metadata in replay; never recompute edits from the current filesystem.

Add `ToolCall::MultiEdit` and the corresponding private-worker Tool label. Advance payload versions for `ToolStarted`, `ToolFinished`, `HookStarted`, `HookFinished`, `GoalPlanContractRecorded`, `ProviderCallRecorded`, `RunStarted` and `AgentSpawned`.

Ship identity migration steps and fixtures following Glob’s precedent: preserve old payload meaning and grants, reject a forged old-version payload containing the new variant, and retain existing migration backup/recovery checks. SQLite schema 5 and checkpoint-store schema need no structural change.

Update Goal scope/success matching, worker mutation validation, checkpoint/rewind attribution, Supervisor edit facts and reporting. Preserve old records’ semantics; migrations must not grant MultiEdit to historical worker assignments.

## Security and resource model

The new Tool/filesystem path triggers the [security pass](/home/fpmirabile/projects/harness-team/wt/plan10/agents/security.md:49).

Assets are Workspace integrity, private worker isolation, journal/checkpoint confidentiality and bounded execution. Model paths, strings and repository bytes are hostile data. Authority remains the admitted directory capability, pinned typed grant and user-owned restrictions; startup trust ordering is unchanged.

- Map MultiEdit to the `Edit` permission family, including path rules, deny-before-ask-before-allow, read-denial protection, Goal scope and `acceptEdits`. Approval cannot widen the grant.
- Hooks remain restrict-only. Show `MultiEdit` with the complete bounded batch; reject unsupported input rewrites rather than treating them as authority.
- Researchers cannot invoke it. Editors require an explicit child capability within the parent grant and existing lease.
- Preserve protected-name, no-follow, hard-link, mount, file-type and directory-identity checks through publication.
- No shell, network, credentials, new descendants or host fallback. Diagnostics expose bounded classifications, not replacement bodies; model output excludes private checkpoint content.

| Resource | Proposed bound and overflow |
|---|---|
| Files / operations | Exactly one file; 1–64 edits; reject empty or excess arrays |
| Replacement content | Sum of all decoded `old_string` and `new_string` bytes ≤64 KiB |
| Arguments | Extend the existing 400 KiB raw and compact serialized ceiling to MultiEdit |
| File buffers | Original, every intermediate and final version ≤1 MiB; check arithmetic before allocation |
| Work | At most 64 bounded transformation passes; no recursive replacement or retry |
| Results / storage | Existing 64 KiB result, observation, catalog, checkpoint, worker and Goal ceilings remain |
| Runtime / concurrency | Existing pinned file-operation deadlines and Guard admission; edits execute sequentially |

## Risks and mitigations

- Partial publication from looping over Edit → transform privately and publish once.
- Replacement expansion exceeding memory bounds → preflight each result with checked arithmetic.
- Permission, checkpoint or worker bypass through an omitted match arm → exhaustively update consumers and extend existing interface corpora.
- False rollback claims after rename → retain `Uncertain`, stop continuation and require explicit inspection before another attempt.
- Peer drift or undocumented behavior → retain versioned citations and distinguish observed behavior from Arany’s proposed guarantees.

## Vertical slices

**One vertical slice, one PR: `feat(tools): add atomic single-file MultiEdit`.**

The smallest usable increment crosses Provider schemas/decoding → catalog and grants → Engine policy/hooks/approval → journal/migrations → Workspace and private-worker Guard execution → receipts/checkpoints/replay → model and terminal presentation.

Ship all contract and evidence changes in that PR. A types-only, parser-only or journal-only PR would not deliver an independently usable capability.

Extend existing evidence owners:

- `tools/tests.rs` and worker corpus: ordering, dependent edits, replace-all, late failure without publication, UTF-8/CRLF fidelity, executable-bit preservation and exact boundary/overflow cases.
- Provider corpora: advertised schema, omitted defaults, escaped 64 KiB content, oversized raw arguments and unavailable-tool refusal across supported routes.
- Permission/Engine corpora: Edit allow/ask/deny, read-denial protection, denied hooks, grant narrowing, researcher rejection and private-worker containment.
- Reducer/migration corpora: malformed receipts, current payload round-trips, forged historical variants, preserved grants, interrupted migration and non-executing replay.
- `guard_runs` and CLI observation owners: real filesystem publication, cancellation/crash stages, checkpoint/rewind, unknown outcomes, exact output and reopened SQLite facts.

## Open questions and phase gates

1. **Research owner:** obtain a primary historical Claude MultiEdit contract for ordering, atomicity and failures, or explicitly close that comparison as unverifiable and remove any parity claim. Until resolved, this remains a provisional plan.
2. **Repository owner:** lift the specific new-typed-Tool freeze in `NEXT_STEPS.md` for MultiEdit. This research request does not authorize implementation.
3. **Repository owner:** accept the proposed existing-file-only contract, sequential replacement semantics and 64-edit ceiling.

**Research → Plan:** close the peer-evidence disposition; retain only substantiated findings and explicit proposed decisions.
**Plan → Implement:** resolve the owner decisions, accept the contract and security model, and restore required skills.
**Implement:** land the single vertical slice; stop and revise research/plan if implementation contradicts its premises.

## Exit criteria

All successful, rejection, failure and recovery observations above pass through their named owners. The Tools and Execution contracts describe the shipped behavior and migration.

Run formatting, Clippy and targeted tests per `docs/development.md`; require complete final-head Linux/macOS CI before handoff. Build only in this worktree’s default `./target` or through the approved cargo wrapper; never set `CARGO_TARGET_DIR`.

Record unavailable native and live Provider evidence in `NEXT_STEPS.md`, preserving the existing 64 KiB Write/Edit checks. Synthetic tests establish neither live compatibility nor native support.

On completion, close the plan under `planning/README.md`, update inbound links and remove only MultiEdit from item 10; retain delete, move, parallel reads and unresolved evidence.