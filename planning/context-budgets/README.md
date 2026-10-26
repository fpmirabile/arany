# Context budgets: remaining output spill slices

## Metadata

- Status: `In progress`
- Owner: harness maintainers
- Last updated: 2026-10-26

## Outcome and scope

Retain bounded private command and MCP output for explicit paged Read without changing inline excerpts, effect outcomes or strict replay. Compaction summary bounds and recent-file hints are complete and belong to [Session](../../docs/specs/session.md#context-and-compaction). Spill behavior belongs to [Tools](../../docs/specs/tools.md#limits-and-failure-behavior), observation fields to [Execution](../../docs/specs/execution.md#persistence-bounds), and private directories to [Credentials](../../docs/specs/credentials.md).

## Remaining vertical slices

Each cumulative patch compiles and keeps existing tests meaningful. Each slice is capped at 600 changed lines. Capture transport comes first because transport plus durable storage exceeds one slice; no unretained reference is committed in S3a.

1. **S3a: bounded private capture transport.** Add the optional observation field, metadata validation, bounded command/MCP capture and private Guard report transport on Linux and macOS. Discard capture before `ToolFinished` until S3b supplies storage. Compile the capture and serialization corpus; preserve inline output and existing replay.
2. **S3b: durable spill and explicit Read.** Admit the StateRoot spill directory, write before `ToolFinished`, retain authority only after acknowledgement, add the spill reference to the folded `ToolFinished`, and add the closed `spill:` Read capability and grammar. Compile private-file admission, Run-byte bounds and unknown-reference evidence. Persistence remains cancellable and deadline-bound; failures preserve the completed effect and inline excerpt.
3. **S3c: cleanup and failure evidence.** Sweep uncommitted files on open while respecting live Session and writer locks. Compile security, write-failure, cancellation, deadline and reopen evidence; update the owning spill contracts and limits.
4. **S3d: retained-output journeys and presentation.** Compile command paging, replay, reopen and cross-Session isolation journeys, including macOS capture assertions. Show only spill metadata in Tool details and `show`.
5. **Later: output-spill storage report.** Add separate `Output spill` rows using the existing private metadata inventory. Exclude the spill subtree from general-record totals; preserve root de-duplication, scan bounds, cancellation and partial reports. Missing spill directories remain absent. No payload reads, retention changes, Tool authority changes or mutations belong here. Owners: [Tools inventory](../../docs/specs/tools.md#local-storage-inventory), [Terminal storage report](../../docs/specs/terminal.md#storage-report), `crates/arany/src/store/state/inventory.rs` and `crates/arany-cli/tests/session_run/storage.rs`.

## Scenarios and verification

- Retained output obeys per-result and per-Run caps, private-file checks and Session scope; guessed and inherited fork references grant no authority.
- Write failure, cancellation and persistence timeout preserve the completed effect, omit the reference and never retry it; cleanup skips live writers and retains committed files.
- Explicit Read pages the committed reference; replay and presentation never read payloads implicitly. Security evidence belongs to `crates/arany/tests/engine_run/workspace_security.rs`; journeys belong to `crates/arany-cli/tests/session_run/tools/native_exec.rs` and `tools.rs`.
- Later reporting counts each identity once without exposing payloads or filenames, changing the journal or creating missing directories.

Verify each cumulative patch with shim formatting, all-targets Clippy, production Clippy with `unwrap_used` denied, and typos. Do not run cargo test for this split. Linux and macOS retain their existing admission adapters; compilation does not establish native evidence.

## Next action and exit

Apply S3a through S3d in order, then implement the separate storage-report slice. Keep this directory and its planning index row until reporting is complete; only then remove the remaining-work entry from NEXT_STEPS.md and close the plan. Preserve links to accepted spill behavior through the owning Tool contract.
