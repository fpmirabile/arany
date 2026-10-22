# Real-scale command snapshot

## Metadata

- **Status:** Draft; Plan gate awaits the exact parser dependency approval below.
- **Owner:** Arany maintainer; implementation owner to be assigned.
- **Last updated:** 2026-10-22.
- **Baseline:** `9f8dc3564f6927c52dd7e2b4292d60dfaee3a964`.
- **Feature:** [NEXT_STEPS.md, Daily use first](../../NEXT_STEPS.md#daily-use-first).
- **Delivery:** Research → Plan → Implement; three ordered vertical slices, one PR each. Typed read discovery is a separate [feature plan](../gitignore-read-discovery/README.md); it reuses the selector introduced here.

## Outcome and scope

Guarded commands work in repositories containing ignored dependency trees, large assets, and substantially more source files than today’s snapshot allows. They retain private execution, bounded disclosure, journal-first effects, and publication exclusively through validated changesets.

This feature changes command input selection, snapshot capacity, and command changeset capacity. It does not add network access, enable worker commands, enlarge worker data limits, or replace the separately approved toolchain/cache workflow.

## Research

### Arany

- [Tools contract](../../docs/specs/tools.md#command-changesets): commands execute against fresh writable copies; capture follows positive descendant termination; Engine journals the observation before separately authorizing and integrating its changeset.
- [Snapshot implementation](../../crates/arany/src/tools/fs.rs): `snapshot_into` uses `collect_root`/`collect` to copy inputs; `command_snapshot`/`command_changeset` subsequently collect and compare private trees. Linux calls these through `process.rs`; macOS captures through `guard/macos/unit.rs`.
- Input copying rejects an ordinary file above 1 MiB. Root discovery omits fixed directory names, including `target` and `node_modules`, but does not interpret `.gitignore`.
- [Current constants](../../crates/arany/src/tools/types.rs): 2,048 retained entries, 32 MiB snapshot content, 8,192 scanned entries, depth 16; command changesets allow 64 files and 8 KiB combined decoded before/after content.
- Increasing those constants globally would also change executable admission, Search, Glob, Skills, and worker-related paths. Command limits must become independently named and enforced.
- `CommandChangeset::valid` additionally enforces a serialized bound below the ordinary 16 KiB argument ceiling. Results are limited to 64 KiB; the Guard task is 1 MiB. These are coupled constraints, not independent knobs.
- [Permission rules](../../docs/specs/tools.md#permission-rules) explicitly distinguish typed Read filtering from subprocess access. Existing Read rules are **not** an OS path filter; the grant and Guard own command disclosure.
- [Persistence](../../docs/specs/execution.md#persistence-bounds) already reserves larger Tool Event envelopes. [Native capacity](../../docs/specs/tools.md#shared-native-capacity-and-settings) separately bounds retained seeds, writable storage, concurrency, and recovery.
- Evidence owners already exist in `tools/tests.rs`, native Guard/admission corpora, `arany-cli/tests/guard_runs.rs`, and reducer/replay tests.
- Reuse existing capability traversal, Guard adapters, publisher, and journal. The [project architecture priorities](../../AGENTS.md#product-and-implementation-priorities) and [Clean Architecture review](../../agents/skills/clean-architecture-review/SKILL.md) favor one private deep module over a new workspace crate.

### Peers

| Peer and inspected revision | Execution and ignore behavior |
|---|---|
| **Codex CLI `rust-v0.117.0`** | Linux uses read-only mounts and writable bind mounts over real paths; macOS applies Seatbelt to execution in the actual working directory. These launch paths do not copy the repository. Filesystem policy, rather than `.gitignore`, controls access. [Linux source](https://raw.githubusercontent.com/openai/codex/rust-v0.117.0/codex-rs/linux-sandbox/src/bwrap.rs), [macOS source](https://raw.githubusercontent.com/openai/codex/rust-v0.117.0/codex-rs/core/src/seatbelt.rs). |
| **Claude Code; sandbox-runtime `c392e6cf9f8df957c66d9ab1461e2cfa99b1ab5d`** | Linux binds allowed real paths beneath a read-only filesystem and masks denied reads; macOS uses Seatbelt path rules. No repository-copy stage appears in these backends. Ignore files do not define sandbox permissions. Claude Code’s exact bundled runtime revision was not established. [Linux source](https://raw.githubusercontent.com/anthropics/sandbox-runtime/c392e6cf9f8df957c66d9ab1461e2cfa99b1ab5d/src/sandbox/linux-sandbox-utils.ts), [macOS source](https://raw.githubusercontent.com/anthropics/sandbox-runtime/c392e6cf9f8df957c66d9ab1461e2cfa99b1ab5d/src/sandbox/macos-sandbox-utils.ts), [official documentation](https://code.claude.com/docs/en/sandboxing). |
| **OpenCode `v1.2.27`** | Bash spawns in the real working directory after permission checks; this path supplies neither copied execution isolation nor an OS sandbox. Its separate shadow-Git snapshots support tracking/restoration, using Git ignore behavior and copied `info/exclude`; they do not restrict shell access. [Bash](https://raw.githubusercontent.com/anomalyco/opencode/v1.2.27/packages/opencode/src/tool/bash.ts), [snapshots](https://raw.githubusercontent.com/anomalyco/opencode/v1.2.27/packages/opencode/src/snapshot/index.ts). |
| **Goose `v1.49.0`** | Developer shell directly spawns the shell with its working directory; no repository-copy or bind-mount boundary exists in that path. No ignore filtering appears in shell launch. Optional container workflows are separate; their exact mount topology was not verified. [Shell source](https://raw.githubusercontent.com/aaif-goose/goose/v1.49.0/crates/goose/src/agents/platform_extensions/developer/shell.rs), [Docker workflow](https://goose-docs.ai/docs/tutorials/goose-in-docker/). |

**Conclusion:** these peer execution paths avoid whole-repository copying, but do not establish Arany’s changeset-only publication guarantee. Keep private copies; document that deliberate divergence in the Tools contract. No peer benchmark or universal absence of limits is claimed.

### Ignore semantics, existing dependencies and the module

[Git's specification](https://git-scm.com/docs/gitignore) defines nested precedence, ordered negation, directory pruning and escaping. An excluded parent prevents descendant re-inclusion; Git normally exempts tracked files. **Decided 2026-10-22 (owner Q1):** use pattern-only selection, including tracked-but-ignored files. Do not read the index to recover them.

The [lockfile](../../Cargo.lock) and [library manifest](../../crates/arany/Cargo.toml) contain **no `globset`, `ignore` or `gix-*` package** in this checkout. Existing pieces include `regex 1.13.1`, `regex-automata 0.4.18`, `regex-syntax 0.8.11`, `aho-corasick 1.1.5`, `memchr 2.8.3`, `walkdir 2.5.0`, `same-file 1.0.6`, `crossbeam-utils 0.8.23` and `log 0.4.34`. [Glob's matcher](../../crates/arany/src/tools/glob.rs) also serves [Search include filters](../../crates/arany/src/tools/search.rs): its 512-byte query, 16 alternatives, 8 KiB translation, 128 KiB regex and 32 KiB DFA bounds do not supply Git-ignore semantics. It rejects negation, anchors, escapes and classes needed here. Permission patterns deliberately have separate restrict-only semantics.

| Option | Assessment |
|---|---|
| Reuse installed `regex` and the query matcher | Keeps the dependency graph, but requires writing the missing Git-ignore parser, precedence and pruning. Reuse capability traversal and query matching, not this incompatible grammar. |
| One new direct dependency: `ignore = "=0.4.33"` | Recommended, subject to the gates below. Use only `GitignoreBuilder::new`, `add_line`, `build` and `matched`; Arany supplies bounded admitted bytes and root-relative names. Never use `WalkBuilder`, `add`, `global`, `build_global` or ambient config discovery. |
| Small in-house matcher | Reject: escapes, slash anchoring, directory rules, nested precedence and negation make this a second Git-ignore implementation. Fewer dependencies do not compensate for disclosure errors. |
| `gix-ignore 0.24.0` / `gix-glob 0.29.0` | Also new dependencies. Their byte matcher has smaller retained state, but recursive wildcard exhaustion returns false rather than an error; treating a missed exclusion as visible is unsafe. Adding complexity restrictions and a work-bound proof is more custom policy than the recommended parser. [Pinned matcher](https://docs.rs/crate/gix-glob/0.29.0/source/src/wildmatch.rs). |

**Dependency review:** `ignore 0.4.33` was published 2026-08-04, is not yanked, and declares Rust 1.88, matching this workspace. It is maintained in ripgrep; source commit `3fce3b5bb0236da2df6d99672afb8a719642eca7`. Its [versioned manifest](https://docs.rs/crate/ignore/0.4.33/source/Cargo.toml) and [source](https://docs.rs/crate/ignore/0.4.33/source/src/gitignore.rs) were inspected. It adds `globset` (reviewed candidate 0.4.20), `bstr` (1.13.1), `crossbeam-deque` (0.8.8) and `crossbeam-epoch` (0.9.21), reusing the existing regex/log/walkdir stack. This is one new **direct** dependency, not one new package. `ignore` and `globset` contain no unsafe blocks or build scripts in the reviewed archives; `bstr` and both new crossbeam packages contain unsafe, and both crossbeam packages contain build scripts. MIT/Unlicense licensing applies to the parser. Pin the resolved lock graph, minimize features and review those scripts, unsafe sites, licenses and current advisories before accepting code; a source scan is not a safety certification. [Versioned packages](https://crates.io/crates/ignore/0.4.33/dependencies).

**Dependency-closure gate:** crates.io's [ignore metadata](https://crates.io/api/v1/crates/ignore/0.4.33/dependencies), [globset metadata](https://crates.io/api/v1/crates/globset/0.4.20/dependencies) and the versioned dependency manifests yield this normal/build closure for Linux/macOS with ordinary features, using the reviewed candidate versions and existing lock versions:

```text
ignore 0.4.33
├── crossbeam-deque 0.8.8
│   ├── crossbeam-epoch 0.9.21 → crossbeam-utils 0.8.23
│   └── crossbeam-utils 0.8.23
├── globset 0.4.20
│   ├── aho-corasick 1.1.5 → memchr 2.8.3
│   ├── bstr 1.13.1 → memchr 2.8.3
│   ├── log 0.4.34
│   ├── regex-automata 0.4.18 → aho-corasick, memchr, regex-syntax
│   └── regex-syntax 0.8.11
├── log 0.4.34
├── memchr 2.8.3
├── regex-automata 0.4.18
├── same-file 1.0.6
└── walkdir 2.5.0 → same-file 1.0.6
```

This is metadata-derived, **not a claimed `cargo tree` run**: ignore is not installed in this workspace. After adding the approved pin, require the actual `cargo tree --locked -p ignore@0.4.33 --edges normal,build` for each Linux/macOS target and `cargo tree --locked -p ignore@0.4.33 --edges features` to reconcile feature unification with this closure before merge. Record/review every difference, including any newly enabled optional dependency. Windows-only `winapi-util` edges and unselected optional dependencies are excluded here. **Crossbeam-deque/epoch are unconditional dependencies even if `WalkBuilder` is never called; disabling default features does not remove them or their unsafe/build-script review.** Do not advertise dead-code elimination as a supply-chain exemption.

**Decided 2026-10-22 (architecture review): no new Arany crate.** Put one deep module privately under `arany::tools`. Its interface accepts bounded immutable ignore-control bytes with their admitted relative directories, and answers excluded/visible for a relative path and file kind, or a typed rejection. It owns ordering, parent pruning, parsing, lifetime and resource accounting; it neither opens files nor authorizes paths. Snapshot, Search, Glob, List, worker-image discovery and Git disclosure adapters are callers. Existing filesystem/image adapters acquire the data; Engine supplies already pinned authority; native adapters enforce resources. Library-specific match/error types never cross into Engine, journal or CLI. No trait or pluggable backend is needed. Deleting this module would duplicate subtle selection logic across those callers, so it earns its depth.

### Shared selector admission

**Ignore sources (single owner for both plans):** admit only workspace-root and nested `.gitignore` files under the pinned directory capability. Controls must be regular, single-link files opened through no-follow handles; symlinks, hardlinks, special files, mount crossings and replacement fail admission. Deny discovery of `.ignore`, `.rgignore`, global/parent gitignore files, repository `core.excludesFile`, and `.git/info/exclude`; never open home/config or invoke Git to find rules. This narrows peer ambient walkers because repository data cannot select trust. Local rules apply even without a `.git` directory. A control ignored by its own patterns remains control data when its directory was reached; it is not automatically model-visible. Apply the caller's grant and, for discovery, its restrictive Read filter before loading controls; command selection uses the command grant; a needed inaccessible/unsafe control rejects generically, while confirmed absence means no rules at that level. Nested controls follow the traversal depth: **16 path components for discovery, 32 for command snapshots**, measured relative to the workspace and including the control filename. There is no extra depth allowance for control loading. Load admitted ancestor rules before a nested query; negation cannot reopen a pruned parent.

**Cheap admission before `add_line` or compilation:** retain the raw ceilings in the resource table. For every nonempty, non-comment candidate rule, count raw metacharacter bytes conservatively, including escaped/class occurrences: at most **16 `*` bytes**, **four non-overlapping `**` pairs**, and **four `{` bytes** per line. Charge expanded pattern bytes with checked arithmetic as `32 × (UTF-8 line bytes + 64)`, at most **64 KiB per line / 1 MiB per operation across all control files**; overflow rejects the operation before handing that rule to the library. This conservative charge covers the pinned byte-regex emitter's escaping, recursive tokens and alternation syntax; it does not implement matching or promise a heap bound. The independent expanded limit can reject a rule below the raw 4 KiB limit. [Emitter at globset 0.4.20](https://docs.rs/crate/globset/0.4.20/source/src/glob.rs), `Tokens::to_regex_with` / `tokens_to_regex` (lines 673–763), emits byte regexes, not an unbounded Cartesian expansion. Pin a corpus assertion that actual emitted size never exceeds the admission charge; a dependency change must re-establish it before rollout. Never truncate, skip a rejected rule, log its text or treat it as a non-match.

**Work bounds:** charge each library matcher invocation, including ancestor checks, before calling it: **1,048,576 calls per discovery operation; 8,388,608 across a command's preparation and capture**. Also impose a wall deadline of **2 seconds for discovery selection, 5 seconds per command selection phase**, each shortened by the owning Tool/Goal deadline. Check before and after calls; the Guard supervisor enforces expiry even inside a library call. Return `ToolDisposition::Limit` with `LimitCause::Resource` for count/expansion exhaustion or `LimitCause::Timeout` for elapsed time. Discard partial results and publish nothing. A timeout is never a visible/nonignored decision; cancellation returns Cancelled and retains ownership until cleanup finishes.

### Matcher placement and interactive latency

**Decided 2026-10-22 (architecture review):** use `ignore` inside the existing resource-owned Guard operation for compilation and walking; keep interactive query refinement in-process over bounded, already-filtered names. Do not launch a Guard per keystroke. A small restricted in-process ignore matcher would either recreate Git-ignore semantics or reject ordinary peer-supported patterns, and using `ignore` in-process still lacks aggregate allocation admission. Preflight and measured RSS reduce risk but do not prove a hard heap limit. This resolves the placement decision without asking the owner to waive a guarantee; dependency approval remains separate.

[Codex's pinned suggestion manager](https://github.com/openai/codex/blob/rust-v0.117.0/codex-rs/tui/src/file_search.rs#L52-L73) starts one search session and calls `session.update_query(&query)` as the token changes; [the library](https://github.com/openai/codex/blob/rust-v0.117.0/codex-rs/file-search/src/lib.rs#L140-L154) updates the query without repeating the walk. Reuse that session/query separation, with the bounded ephemeral suggestion snapshot defined in the [discovery plan](../gitignore-read-discovery/README.md#interactive-admission-and-latency). Claude's documentation does not establish a comparable internal latency mechanism; no claim is made about it. Arany's resource-owned preparation is a deliberate isolation/bounds divergence, not evidence of peer-equivalent speed.

The [globset compiler](https://docs.rs/crate/globset/0.4.20/source/src/lib.rs) has 10 MiB NFA and hybrid-cache limits, excluding parser state, other strategies, temporaries and pools. Keep all allocation-heavy construction, matching and command capture inside the unit, after admission. Use the existing **512 MiB default-unit envelope** for these phases, single-threaded; Build preparation/capture must not inherit a larger payload allowance. Share [native capacity](../../crates/arany/src/tools/guard/admission.rs): eight units by default, configurable up to 64, hence **4 GiB / 32 GiB aggregate admitted phase memory**. The 16 in-process read slots add no parser units; exact reads remain there. Linux cgroups and macOS supervised physical-footprint thresholds keep their documented differences; macOS sampling is not an instantaneous allocation ceiling.

**Unit wait:** discovery waits at most **250 ms** from request admission, including lock acquisition/recovery; `@` uses **100 ms**. A full queue or wait expiry returns typed `ToolError::QueueFull` (busy), mapped for model discovery to `Limit / ReadCapacity`, with no dispatch and no automatic retry. Do not conflate it with a matching timeout. Command preparation waits at most **30 seconds**, shortened by its existing preparation/Goal deadline, and retains the existing resource-limit outcome. Clamp the admission helper's recovery work and ticket lifetime to that deadline; dropping a timed-out waiter must remove its ticket without releasing any live unit early. Preserve the existing FIFO and queue ceiling. This is proposed behavior: the present helper returns generic Limit on expiry and can spend up to 30 seconds recovering capacity.

**Measured implementation gate:** extend the existing Guard resource corpus with a fixed adversarial corpus covering maximal admitted stars/recursive patterns/classes/alternations, near-limit UTF-8 lines, 256 controls distributed across admitted nested/sibling directories, complete live matcher retention and cancellation during compile/match. Record the **maximum observed whole-unit RSS on Linux and physical footprint on macOS**, including compiler transients; require **at most 256 MiB per phase** in this corpus, leaving half the 512 MiB envelope as headroom. Measure both one unit and default-eight-unit saturation with no throughput/RSS extrapolation presented as evidence. Separately test the configured-maximum reservation arithmetic and actual resource-limit termination. The finite corpus is a measured worst-case fixture, not proof of a mathematical worst case; native enforcement remains mandatory. If memory or the discovery latency gate fails, tighten admission or return to Research, never silently raise limits or move parsing into the controller. These are implementation/CI gates, not tests run for this documentation change.

No daemon, new workspace crate, unsafe allocator or per-path IPC is needed. The first command slice must put parser phases under enforcement before parsing and prove termination/cleanup on both native adapters; an unsupported platform fails closed. Do not describe the current preparation path as already protected by the payload budget.

## Contract and scenarios

**Owner:** [Tools: commands, changesets, limits, and native capacity](../../docs/specs/tools.md). Execution owns any changed persistence semantics.

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

- Use [shared selector admission](#shared-selector-admission); freeze admitted control bytes and selection decisions for the invocation.
- Apply ancestor/nested precedence, comments, escapes, directory rules, and negation. Ignore matching is case-sensitive on both platforms.
- Decided 2026-10-22 (owner Q1): apply patterns without querying the Git index; tracked-but-ignored files are omitted too.
- Keep fixed exclusions and grant restrictions dominant. Negation removes an ignore exclusion only; it never grants access.
- Skip oversized ordinary input files after safe metadata admission, without reading their contents. Keep a bounded omission record so capture cannot confuse omission with absence.
- Scan the entire private output tree structurally after descendants stop. Reject unsafe objects even inside ignored directories; never prune output validation merely because a path is ignored.
- Decided 2026-10-22 (owner Q2): discard newly generated ignored regular files after structural validation, using frozen rules, and report their count. Do not read their content merely to discard it. Always capture changes to baseline files. Reject attempts to publish at oversized-input omission paths.
- A newly oversized publishable output or enlarged baseline file rejects capture; “skip over 1 MiB” applies to input selection, not silent loss of command edits.
- Report separate counts of observed ignored entries/pruned directory roots, oversized input files, and discarded output files. Never claim the number of descendants of an unvisited directory. Counts and diagnostics contain neither contents nor denied-path names.

Tools owns selection and capture invariants; native adapters own containment/storage; Engine owns ordering and integration authorization; the reducer owns replay validation. Keep the selector interface private within the library and reuse the existing Guard process protocol for resource-owned operations, with Linux/macOS as the concrete enforcement adapters.

## Security and resource model

**Assets:** host workspace integrity, denied file contents, private State/credentials, journal integrity, and bounded host resources. Attackers control paths, ignore patterns, file metadata, repository churn, and command output.

**Authority:** the pinned grant, normalized intent, deterministic Policy, and effective Guard attestation remain authoritative. Startup admits trusted configuration and directories before repository data. Children retain their existing narrower limits.

No live workspace mount is introduced into the payload, including read-only “convenience” mounts for omitted files. Resource-owned preparation reads only through its admitted source capability and closes that access before payload GO; commands and descendants see only admitted private inputs and separately authorized runtime resources. Ignoring a path never creates host write-through access.

Decided 2026-10-22 (owner Q3): accept 16,384 retained entries, 128 MiB copied content and 32 KiB decoded command changesets. Supporting traversal/storage/envelope limits below make those ceilings coherent; dependency approval remains the open gate. These are product bounds, not measured capacity claims:

| Resource | Proposed bound and overflow |
|---|---|
| Primary command snapshot | **16,384 retained entries; 128 MiB copied content**. Reject the invocation on aggregate overflow; never run with an arbitrary truncated subset. |
| Traversal | **131,072 scanned entries; depth 32; 16 MiB aggregate retained path/metadata accounting**. Charge entries before filtering; prune ignored input directories. Overflow rejects. |
| Individual input file | **1 MiB inclusive**; larger safe regular files are omitted. Ignore-control files follow their stricter limits below. |
| Ignore input | **32 KiB/file, 256 KiB aggregate, 256 files, 4,096 rules, 4 KiB/line**. Invalid encoding, unsafe control files, parser failure, or overflow rejects before payload dispatch. |
| Matcher resources | [Shared admission](#shared-selector-admission) bounds preflight expansion and match calls/time; [placement](#matcher-placement-and-interactive-latency) bounds unit wait and aggregate memory. Fail whole on overflow; no in-process heap claim. |
| Default writable project | **256 MiB**, separate from unchanged 64 MiB scratch; Build retains its existing maximum of 1 GiB. Lower effective capacity rejects before dispatch where determinable. |
| Command changeset | **64 files; 32 KiB aggregate decoded before/after content; 48 KiB serialized changeset**. Any exceeded dimension rejects the whole capture. |
| Integration arguments | **64 KiB for internal command-receipt ApplyChangeset only**; ordinary model command arguments remain 16 KiB. |
| Results and transport | Keep **64 KiB result**, existing Tool Event ceilings, and **1 MiB Guard task**. Fit the complete changeset and summary before stream excerpts; verify the actual serialized envelopes. |
| Aggregate native storage | Linux seed payload becomes at most **128 + 96 = 224 MiB/unit**, or **1.75 GiB at the default eight units / 14 GiB at the configured maximum 64**, excluding separately bounded metadata. macOS reserves actual enlarged backing volumes. |
| Concurrency/time | Keep existing unit/FIFO limits and finite preparation, payload, capture, cleanup, and Goal deadlines. No retry or automatic unguarded fallback. |

Stream input copying and post-command comparison where possible; avoid simultaneously retaining two complete content trees. Native memory enforcement remains independent: fitting the snapshot ceiling does not promise sufficient memory for every payload.

The 32 KiB changeset proposal deliberately fits existing journal/result architecture. It enables modest multi-file edits, not repository-wide formatter output. Larger publication batches are outside this feature. The 32 KiB decoded, 48 KiB serialized and 64-file bounds are independent: a batch at one ceiling can still fail another. Reserve metadata and escaped-path overhead rather than assuming all maxima fit together.

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
- Decided 2026-10-22 (owner): no compatibility design for old defaults, saved settings or receipts. Keep one current format. If implementation changes a stored format version, the repository's existing migration/fixture rule still applies; do not add compatibility readers or an optional-field strategy just to preserve old defaults.
- Commit omission/discard summaries in the canonical command observation so live output and replay agree. Persist counts and outcomes, not raw ignore files or a reusable authority cache.
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
   Committed command intent → resource admission → capability-safe ignore-control loading → bounded ignore parsing → private copy → Guard execution → complete output validation → journaled result → existing integration/presentation. Ship the shared selector with the chosen resource placement, frozen-rule semantics, omission collision handling, and count summaries while retaining current snapshot ceilings. No parser-only PR: the command path must be usable end to end. Update Tools scenarios. Extend existing snapshot/changeset and Guard journeys with nested rules, oversized assets, malicious ignore files, changed ignore rules, unsafe ignored output, and replay.

2. **PR 2 — Commands execute against larger bounded repositories.**  
   Engine’s pinned resource values → native capacity reservation → streamed snapshot → command/capture → cleanup → committed receipt and replay. Ship independent 16,384-entry/128 MiB command limits, larger traversal bounds, and default project storage on both adapters. Update Tools limits and retained-storage accounting. Test successful repositories above each old ceiling, exact/new overflow boundaries, storage exhaustion, cancellation, and retained-unit recovery; pin unrelated limits unchanged.

3. **PR 3 — Commands publish larger bounded changesets.**  
   Capture → committed command observation → Engine scope/permission decision → approval → guarded publication → receipt/checkpoint → presentation and strict replay. Ship 32 KiB decoded/48 KiB serialized changesets and the internal integration argument allowance together. Test binary content, long escaped paths, executable changes, nonzero exits with valid changes, conflict-before-write, partial-publication uncertainty, serialization overflow, and insufficient Store/context headroom. Update owning contracts and complete feature closeout.

Each PR must leave its shipped behavior usable and preserve all five guarantees. Do not create separate “types,” “wiring,” or “tests” PRs.

## Adversarial design decisions

- **Can an ignored file still contain secrets?** Yes. Decided 2026-10-22 (owner Q2 addition): prune input directories and omit file content before copying, not merely before returning results. `.gitignore` is a discovery preference, never a secret-access grant. Explicit typed Read follows peer permission behavior in the separate plan; shell snapshot omissions have no automatic escape hatch.
- **Can a control file bypass Read restrictions?** No; [shared selector admission](#shared-selector-admission) owns this rule. Commands still use the command grant, not an invented OS interpretation of typed Read rules.
- **What freezes?** Retain admitted control bytes/identities and selection for the invocation. Recheck identities during preparation; detected replacement rejects rather than mixing rules. Rule changes by the payload apply to its next invocation only. No atomic repository snapshot against hostile same-user churn is claimed.
- **Can output hide behind ignore rules?** No. Structurally traverse the entire private project after positive descendant absence, including ignored directories, charging every visited entry. Reject unsafe objects and overflow before content comparison; prune only input traversal. Baseline membership wins over ignore rules. Newly generated ignored regular bytes are discarded, not hashed into a publishable edit.
- **Can omission become deletion or overwrite?** Baseline absence is not a deletion. Keep bounded omitted-file facts and pruned prefixes; publication still checks every proposed create against host absence. A skipped host file or unsafe parent cannot become a new writable target. Do not enumerate ignored host subtrees just to count them.
- **What if copying grows, capture fails or cancellation races?** At 1 MiB input is admitted; growth beyond the bounded read rejects that input/copy rather than accepting a partial file. Preparation failure prevents dispatch; capture failure prevents integration. Publication uncertainty stops continuation without retry. Existing private-unit recovery owns orphan cleanup.
- **Can replay consult the current `.gitignore`?** No. The reducer consumes committed results and receipts; selector data never becomes replay authority. Pin revised enforcement algorithms in both native digests. Check command, integration, approval, checkpoint and context envelopes together in slice 3.
- **Are the slices and OS claim honest?** Each PR crosses execution, journal, presentation, spec and its existing evidence owners. Linux and macOS share selection; resource admission, alias checks, termination and cleanup need native evidence. The larger constants do not silently enlarge worker, Search, Glob, Skill or executable limits.

## Open questions and gates

1. **Dependency approval — owner:** Git-ignore semantics require a new parser and the unconditional transitive code described above. **Recommend approving `ignore = "=0.4.33"` with the pinned dependency review gate; no new workspace crate.**

Owner Q1–Q3 product choices are settled. The placement decision is settled by the bounded Guard operation and in-process suggestion refinement above; its RSS, queue, cancellation and latency gates are implementation evidence, not a request to relax guarantees. Set this plan to `Ready` after dependency approval. A failed gate or source contradiction returns the affected design to Research before shipping.

## Exit criteria

- Existing snapshot/changeset corpora prove selection, exact boundaries, rejection-before-publication, and unchanged unrelated limits.
- Guard journeys prove real execution, isolation, discarded ignored output, collision refusal, cleanup, and committed observations on each claimed platform.
- Closed-store replay proves identical outcomes without filesystem discovery or effect re-execution; migration fixtures are required if any version changes.
- Run targeted formatting, Clippy, and affected tests; complete CI must pass on each final PR head. Build only in this worktree or through the approved Cargo wrapper.
- Measure representative repository preparation/capture time and peak resources; record host/profile details before making performance claims.
- Keep unavailable native and owner dogfooding checks in `NEXT_STEPS.md`. Update behavior once at its owning spec and close the plan under `planning/README.md` when implemented.
