# Real-scale command snapshot

## Metadata

- **Status:** Draft; research complete for the cited paths, Plan gate awaiting owner decisions below.
- **Owner:** Arany maintainer; implementation owner to be assigned.
- **Last updated:** 2026-10-21.
- **Baseline:** `9f8dc3564f6927c52dd7e2b4292d60dfaee3a964`.
- **Feature:** [NEXT_STEPS.md, Daily use first, item 7](/home/fpmirabile/projects/harness-team/wt/plan7/NEXT_STEPS.md:24).
- **Delivery:** Research → Plan → Implement; three ordered vertical slices, one PR each. This document is conversation-only; no files were edited.

## Outcome and scope

Guarded commands work in repositories containing ignored dependency trees, large assets, and substantially more source files than today’s snapshot allows. They retain private execution, bounded disclosure, journal-first effects, and publication exclusively through validated changesets.

This feature changes command input selection, snapshot capacity, and command changeset capacity. It does not add network access, enable worker commands, enlarge worker data limits, or replace the separately approved toolchain/cache workflow.

## Research

### Arany

- [Tools contract](./docs/specs/tools.md#command-changesets): commands execute against fresh writable copies; capture follows positive descendant termination; Engine journals the observation before separately authorizing and integrating its changeset.
- [Snapshot implementation](./crates/arany/src/tools/fs.rs): `snapshot_into` uses `collect_root`/`collect` to copy inputs; `command_snapshot`/`command_changeset` subsequently collect and compare private trees. Linux calls these through `process.rs`; macOS captures through `guard/macos/unit.rs`.
- Input copying rejects an ordinary file above 1 MiB. Root discovery omits fixed directory names, including `target` and `node_modules`, but does not interpret `.gitignore`.
- [Current constants](./crates/arany/src/tools/types.rs): 2,048 retained entries, 32 MiB snapshot content, 8,192 scanned entries, depth 16; command changesets allow 64 files and 8 KiB combined decoded before/after content.
- Increasing those constants globally would also change executable admission, Search, Glob, Skills, and worker-related paths. Command limits must become independently named and enforced.
- `CommandChangeset::valid` additionally enforces a serialized bound below the ordinary 16 KiB argument ceiling. Results are limited to 64 KiB; the Guard task is 1 MiB. These are coupled constraints, not independent knobs.
- [Permission rules](./docs/specs/tools.md#permission-rules) explicitly distinguish typed Read filtering from subprocess access. Existing Read rules are **not** an OS path filter; the grant and Guard own command disclosure.
- [Persistence](./docs/specs/execution.md#persistence-bounds) already reserves larger Tool Event envelopes. [Native capacity](./docs/specs/tools.md#shared-native-capacity-and-settings) separately bounds retained seeds, writable storage, concurrency, and recovery.
- Evidence owners already exist in `tools/tests.rs`, native Guard/admission corpora, `arany-cli/tests/guard_runs.rs`, and reducer/replay tests.
- Ponytail’s [repository-pinned source](https://github.com/DietrichGebert/ponytail/blob/e3ba2aa6f1e6f0bc4d69eb09c9f0d0a93af56156/skills/ponytail/SKILL.md) was read remotely because the generated local installation is absent. Reuse existing capability traversal, Guard adapters, publisher, and journal.

### Peers

| Peer and inspected revision | Execution and ignore behavior |
|---|---|
| **Codex CLI `rust-v0.117.0`** | Linux uses read-only mounts and writable bind mounts over real paths; macOS applies Seatbelt to execution in the actual working directory. These launch paths do not copy the repository. Filesystem policy, rather than `.gitignore`, controls access. [Linux source](https://raw.githubusercontent.com/openai/codex/rust-v0.117.0/codex-rs/linux-sandbox/src/bwrap.rs), [macOS source](https://raw.githubusercontent.com/openai/codex/rust-v0.117.0/codex-rs/core/src/seatbelt.rs). |
| **Claude Code; sandbox-runtime `c392e6cf9f8df957c66d9ab1461e2cfa99b1ab5d`** | Linux binds allowed real paths beneath a read-only filesystem and masks denied reads; macOS uses Seatbelt path rules. No repository-copy stage appears in these backends. Ignore files do not define sandbox permissions. Claude Code’s exact bundled runtime revision was not established. [Linux source](https://raw.githubusercontent.com/anthropics/sandbox-runtime/c392e6cf9f8df957c66d9ab1461e2cfa99b1ab5d/src/sandbox/linux-sandbox-utils.ts), [macOS source](https://raw.githubusercontent.com/anthropics/sandbox-runtime/c392e6cf9f8df957c66d9ab1461e2cfa99b1ab5d/src/sandbox/macos-sandbox-utils.ts), [official documentation](https://code.claude.com/docs/en/sandboxing). |
| **OpenCode `v1.2.27`** | Bash spawns in the real working directory after permission checks; this path supplies neither copied execution isolation nor an OS sandbox. Its separate shadow-Git snapshots support tracking/restoration, using Git ignore behavior and copied `info/exclude`; they do not restrict shell access. [Bash](https://raw.githubusercontent.com/anomalyco/opencode/v1.2.27/packages/opencode/src/tool/bash.ts), [snapshots](https://raw.githubusercontent.com/anomalyco/opencode/v1.2.27/packages/opencode/src/snapshot/index.ts). |
| **Goose `v1.49.0`** | Developer shell directly spawns the shell with its working directory; no repository-copy or bind-mount boundary exists in that path. No ignore filtering appears in shell launch. Optional container workflows are separate; their exact mount topology was not verified. [Shell source](https://raw.githubusercontent.com/aaif-goose/goose/v1.49.0/crates/goose/src/agents/platform_extensions/developer/shell.rs), [Docker workflow](https://goose-docs.ai/docs/tutorials/goose-in-docker/). |

**Conclusion:** these peer execution paths avoid whole-repository copying, but do not establish Arany’s changeset-only publication guarantee. Keep private copies; document that deliberate divergence in the Tools contract. No peer benchmark or universal absence of limits is claimed.

### Ignore semantics and parser

Git defines nested precedence, ordered negation, directory pruning, escaping, and a tracked-file exception. Ignored parent directories prevent descendant re-inclusion. [Git specification](https://git-scm.com/docs/gitignore).

No `ignore`/`globset` dependency is currently installed. Existing Arany matchers deliberately reject parts of `.gitignore` syntax. Prefer the reviewed `ignore` crate’s data-only `GitignoreBuilder::add_line` over a second Gitignore implementation; never use its ambient filesystem discovery APIs. Its underlying compiler has finite per-matcher limits, but these do not establish an aggregate budget across nested matchers. [Parser source, ripgrep 15.1.0](https://raw.githubusercontent.com/BurntSushi/ripgrep/15.1.0/crates/ignore/src/gitignore.rs), [compiler bounds](https://raw.githubusercontent.com/BurntSushi/ripgrep/15.1.0/crates/globset/src/lib.rs).

## Contract and scenarios

**Owner:** [Tools: commands, changesets, limits, and native capacity](./docs/specs/tools.md). Execution owns any changed persistence semantics.

1. A command reads admitted source in a repository exceeding today’s file/byte limits and publishes an approved small edit.
2. Ignored input trees and ordinary files above 1 MiB are absent from the private copy; their absence does not abort unrelated commands.
3. Exactly 1 MiB remains admissible. A file growing during bounded copying cannot exceed the cap or produce an accepted partial file.
4. Skipped input is never represented as a deletion. A command cannot overwrite a skipped host file by presenting its private replacement as a new file.
5. Nested ignore rules and negation affect selection only within admitted paths. They cannot restore protected, denied, linked, or out-of-grant input.
6. Changing `.gitignore` during execution cannot hide modifications to baseline files or change that command’s capture policy.
7. Capture, quota, conflict, cancellation, and cleanup failures publish nothing; uncertain publication stops continuation without retry.
8. Replay reproduces committed outcomes without reading current ignore files or executing commands.

## Chosen direction

Retain the existing disposable-copy architecture and separate **input selection** from **output validation**.

- Snapshot only repository-local `.gitignore` data encountered through admitted no-follow directory capabilities. Freeze those bytes and selection decisions for the invocation.
- Apply ancestor/nested precedence, comments, escapes, directory rules, and negation. Ignore matching is case-sensitive on both platforms.
- Do not consult parent repositories, global Git configuration, `.git/info/exclude`, `.ignore`, hooks, filters, or repository-controlled executables. Do not invoke Git for snapshot discovery.
- Recommended initial semantics apply `.gitignore` patterns without querying the Git index; tracked-but-ignored files are consequently omitted too. This explicit divergence requires owner acceptance.
- Keep fixed exclusions and grant restrictions dominant. Negation removes an ignore exclusion only; it never grants access.
- Skip oversized ordinary input files after safe metadata admission, without reading their contents. Keep a bounded omission record so capture cannot confuse omission with absence.
- Scan the entire private output tree structurally after descendants stop. Reject unsafe objects even inside ignored directories; never prune output validation merely because a path is ignored.
- Recommended output policy: discard newly generated ignored regular files after validation, using the frozen rules. Always capture changes to baseline files. Reject attempts to publish at oversized-input omission paths.
- A newly oversized publishable output or enlarged baseline file rejects capture; “skip over 1 MiB” applies to input selection, not silent loss of command edits.
- Report bounded omission/discard counts without file contents or denied-path names.

Tools owns selection and capture invariants; native adapters own containment/storage; Engine owns ordering and integration authorization; the reducer owns replay validation. Keep these interfaces in-process and private, with Linux/macOS as the existing concrete enforcement adapters.

## Security and resource model

**Assets:** host workspace integrity, denied file contents, private State/credentials, journal integrity, and bounded host resources. Attackers control paths, ignore patterns, file metadata, repository churn, and command output.

**Authority:** the pinned grant, normalized intent, deterministic Policy, and effective Guard attestation remain authoritative. Startup admits trusted configuration and directories before repository data. Children retain their existing narrower limits.

No live workspace mount is introduced, including read-only “convenience” mounts for omitted files. Commands and descendants see only admitted private inputs and separately authorized runtime resources. Ignoring a path never creates host write-through access.

Proposed ceilings are product choices pending the Plan gate, not measured capacity claims:

| Resource | Proposed bound and overflow |
|---|---|
| Primary command snapshot | **16,384 retained entries; 128 MiB copied content**. Reject the invocation on aggregate overflow; never run with an arbitrary truncated subset. |
| Traversal | **131,072 scanned entries; depth 32; 16 MiB aggregate retained path/metadata accounting**. Charge entries before filtering; prune ignored input directories. Overflow rejects. |
| Individual input file | **1 MiB inclusive**; larger safe regular files are omitted. Ignore-control files follow their stricter limits below. |
| Ignore input | **32 KiB/file, 256 KiB aggregate, 256 files, 4,096 rules, 4 KiB/line**. Invalid encoding, unsafe control files, parser failure, or overflow rejects before payload dispatch. |
| Matcher resources | Require a reviewed finite aggregate allocation bound, not just per-pattern limits; release traversal-local matchers promptly and check cancellation/deadlines. The exact enforceable aggregate is a Plan-gate question below. |
| Default writable project | **256 MiB**, separate from unchanged 64 MiB scratch; Build retains its existing maximum of 1 GiB. Lower effective capacity rejects before dispatch where determinable. |
| Command changeset | **64 files; 32 KiB aggregate decoded before/after content; 48 KiB serialized changeset**. Any exceeded dimension rejects the whole capture. |
| Integration arguments | **64 KiB for internal command-receipt ApplyChangeset only**; ordinary model command arguments remain 16 KiB. |
| Results and transport | Keep **64 KiB result**, existing Tool Event ceilings, and **1 MiB Guard task**. Fit the complete changeset and summary before stream excerpts; verify the actual serialized envelopes. |
| Aggregate native storage | Linux seed payload becomes at most **128 + 96 = 224 MiB/unit**, or **1.75 GiB at eight units**, excluding separately bounded metadata. macOS reserves actual enlarged backing volumes. |
| Concurrency/time | Keep existing unit/FIFO limits and finite preparation, payload, capture, cleanup, and Goal deadlines. No retry or automatic unguarded fallback. |

Stream input copying and post-command comparison where possible; avoid simultaneously retaining two complete content trees. Native memory enforcement remains independent: fitting the snapshot ceiling does not promise sufficient memory for every payload.

The 32 KiB changeset proposal deliberately fits existing journal/result architecture. It enables modest multi-file edits, not repository-wide formatter output. Larger publication batches require a separately resolved transport/context design within this feature.

**Security pass:** review no-follow and mount checks, case aliases, hard links, metadata replacement, parser allocation, dependency provenance, disclosure canaries, retained storage, cancellation, and descendant cleanup. A failed unsafe-path check must not be converted into an ignorable oversized-file case.

**Non-claims:** no atomic multi-file publication, protection against root/hostile same-user mutation, unlimited repository support, hard-real-time filesystem response, or new native-platform support claim.

## Linux and macOS impact

- **Linux:** retain private namespaces, tmpfs, cgroups, owned seed recovery, and positive descendant absence. Update seed traversal/recovery bounds and enforcement digest alongside command limits.
- **macOS:** retain case-sensitive private images, Seatbelt, coalition cleanup, and pre-detach capture. Enlarge default project backing storage and reserve it before attachment.
- Both platforms use identical selection semantics. Host case/Unicode aliases remain subject to admission/publication checks.
- Build and runtime grants remain explicit. No host dependency directory becomes readable merely because it is ignored.
- Native Linux and macOS tests are required for support claims; synthetic evidence alone establishes only implementation behavior.

## Stored formats and Events

No new Event kind, durable snapshot store, or artifact subsystem is proposed.

- Keep the intent → command observation → integration intent → integration observation sequence and receipt correlation.
- Bind changed algorithms and ceilings into both native enforcement digests.
- Preserve valid historical numeric limits and existing record bytes; new live defaults must not make historical 64 MiB project receipts invalid.
- Any omission summary added to command output should use an optional field omitted at its default, preserving old serialization and hashes under the [additive-field rule](./docs/specs/execution.md#admission-persistence-and-recovery).
- **No migration is expected for these compatible bound/default changes.** If implementation needs required fields, changes existing record meaning, or rejects old records, stop and add the affected Event/record version bump, migration step, and fixture. Never rewrite old receipts to claim larger authority.
- Recheck Event headroom, context reservation, checkpoint capture, approval rendering, Supervisor processing, and Guard frame fitting for the enlarged integration.

## Risks and mitigations

- **Ignore rules hide writes:** freeze rules; baseline membership overrides ignore matching; validate the complete output tree.
- **Omitted file becomes an accidental overwrite:** retain omission identity/path facts and reject publication collisions.
- **Parser or traversal denial of service:** bound input before allocation, compilation, and enumeration; reject partial parsing; keep errors free of hostile text.
- **Raised constants widen unrelated Tools:** introduce command-specific bounds and retain existing Search, Skills, executable, and worker contracts.
- **Higher storage/memory pressure:** stream copies/comparison, account metadata and retained units, and test saturation and crash recovery.
- **Ignored assets are required by a command:** report omission honestly; do not fetch, mount, or widen access automatically.

## Vertical slices

1. **PR 1 — Commands safely omit ignored and oversized inputs.**  
   Input admission → bounded ignore parsing → private copy → Guard execution → complete output validation → journaled result → existing integration/presentation. Ship frozen-rule semantics, omission collision handling, and count summaries while retaining current aggregate ceilings. Update Tools scenarios. Extend existing snapshot/changeset and Guard journeys with nested rules, oversized assets, malicious ignore files, changed ignore rules, unsafe ignored output, and replay.

2. **PR 2 — Commands execute against larger bounded repositories.**  
   Engine’s pinned resource values → native capacity reservation → streamed snapshot → command/capture → cleanup → committed receipt and replay. Ship independent 16,384-entry/128 MiB command limits, larger traversal bounds, and default project storage on both adapters. Update Tools limits and retained-storage accounting. Test successful repositories above each old ceiling, exact/new overflow boundaries, storage exhaustion, cancellation, and retained-unit recovery; pin unrelated limits unchanged.

3. **PR 3 — Commands publish larger bounded changesets.**  
   Capture → committed command observation → Engine scope/permission decision → approval → guarded publication → receipt/checkpoint → presentation and strict replay. Ship 32 KiB decoded/48 KiB serialized changesets and the internal integration argument allowance together. Test binary content, long escaped paths, executable changes, nonzero exits with valid changes, conflict-before-write, partial-publication uncertainty, serialization overflow, and insufficient Store/context headroom. Update owning contracts and complete feature closeout.

Each PR must leave its shipped behavior usable and preserve all five guarantees. Do not create separate “types,” “wiring,” or “tests” PRs.

## Open questions and gates

**Research gate:** cited current paths and peer mechanisms are verified from source; no native execution or scale benchmark was performed.

**Plan gate — owner decisions required before implementation:**

1. **Product owner:** accept pattern-only selection, including omission of tracked-but-ignored files, or require a neutralized, bounded tracked-file exception?
2. **Product/security owner:** accept discarding newly generated ignored regular files with a visible count, while always validating structure and capturing baseline changes?
3. **Product owner:** accept the proposed repository/storage ceilings and 32 KiB publication limit, or require larger changesets and an expanded journal/context design?
4. **Security/implementation owner:** approve the exact parser dependency and an enforceable aggregate matcher-memory bound after dependency review. Upstream’s per-matcher bounds alone do not close this question.

**Implement gate:** resolve and record those choices first. If implementation contradicts research or envelope calculations, return to Plan before proceeding.

## Exit criteria

- Existing snapshot/changeset corpora prove selection, exact boundaries, rejection-before-publication, and unchanged unrelated limits.
- Guard journeys prove real execution, isolation, discarded ignored output, collision refusal, cleanup, and committed observations on each claimed platform.
- Closed-store replay proves identical outcomes without filesystem discovery or effect re-execution; migration fixtures are required if any version changes.
- Run targeted formatting, Clippy, and affected tests; complete CI must pass on each final PR head. Build only in this worktree or through the approved Cargo wrapper.
- Measure representative repository preparation/capture time and peak resources; record host/profile details before making performance claims.
- Keep unavailable native and owner dogfooding checks in `NEXT_STEPS.md`. Update behavior once at its owning spec and close the plan under `planning/README.md` when implemented.
- **This task’s verification:** read-only code, contract, and primary-source research; no builds, tests, benchmarks, or file edits.