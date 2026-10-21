# Search with regex, file filtering and paging

## Metadata

- Status: `Draft`
- Owner: Arany maintainers
- Last updated: `2026-10-21`

## Outcome and scope

Extend the existing read-only `search` Tool with regex matching, an `include` glob, explicit case control and paging of at most 100 matching lines per call. A caller can find code beyond today's first 64 rows through correlated Tool calls without launching a process. Keep one row per matching line; multiline search, count/files-only modes and `.gitignore` discovery are outside this feature.

## Research

Verified against this worktree on 2026-10-21.

| Finding | Source |
|---|---|
| Search currently accepts `{path, query}` with a nonempty query of at most 512 bytes; matching uses literal `line.contains`, one-based line numbers and previews cut to 512 characters. | [crates/arany/src/tools/fs.rs:25](../../crates/arany/src/tools/fs.rs), [fs.rs:74](../../crates/arany/src/tools/fs.rs), [fs.rs:1192](../../crates/arany/src/tools/fs.rs) |
| Both scanning and fitted output stop at 64 rows; the JSON result has `entries`, `limit` and `may_be_truncated`. Private-worker search also calls the shared search/fit helpers and stops at 64. | [crates/arany/src/tools/fs.rs:1170](../../crates/arany/src/tools/fs.rs), [fs.rs:1215](../../crates/arany/src/tools/fs.rs), [crates/arany/src/tools/worker.rs:1100](../../crates/arany/src/tools/worker.rs) |
| The direct dependency is inherited by arany, pinned to `regex = "=1.13.1"`, and resolved to 1.13.1. Existing permission regex compilation already sets size, DFA-cache and nesting limits. | [crates/arany/Cargo.toml:27](../../crates/arany/Cargo.toml), [Cargo.toml:45](../../Cargo.toml), [Cargo.lock:2861](../../Cargo.lock), [crates/arany/src/tools/permissions.rs:805](../../crates/arany/src/tools/permissions.rs) |
| Current traversal is bounded by 2,048 retained entries, 8,192 scanned directory entries, depth 16, 1 MiB per collected file and 32 MiB total content; in-process reads share 16 slots. | [crates/arany/src/tools/types.rs:24](../../crates/arany/src/tools/types.rs), [fs.rs:647](../../crates/arany/src/tools/fs.rs), [fs.rs:671](../../crates/arany/src/tools/fs.rs), [fs.rs:697](../../crates/arany/src/tools/fs.rs), [fs.rs:726](../../crates/arany/src/tools/fs.rs), [crates/arany/src/tools.rs:1223](../../crates/arany/src/tools.rs) |
| Protected components include `.env`, case-insensitive protected names and `.env.*`; path validation is bounded and rejects traversal. | [crates/arany/src/tools/types.rs:326](../../crates/arany/src/tools/types.rs), [types.rs:358](../../crates/arany/src/tools/types.rs), [types.rs:373](../../crates/arany/src/tools/types.rs) |
| Whole-project enumeration also skips `target`, `node_modules`, `.venv` and `venv`. | [crates/arany/src/tools/fs.rs:741](../../crates/arany/src/tools/fs.rs), [fs.rs:769](../../crates/arany/src/tools/fs.rs) |
| Search hooks can currently narrow a subtree only while retaining the query. Read and Grep permission mappings already cover search. | [crates/arany/src/tools/permissions.rs:345](../../crates/arany/src/tools/permissions.rs), [Tools permission rules](../../docs/specs/tools.md#permission-rules), [hook narrowing](../../docs/specs/tools.md#hook-execution) |

Peer sources checked: [OpenCode grep input and execution](https://github.com/anomalyco/opencode/blob/dev/packages/core/src/tool/plugin/grep.ts#L13-L33) expose regex/literal selection, include, case and limit; [its filesystem defaults](https://github.com/anomalyco/opencode/blob/dev/packages/core/src/filesystem.ts#L62-L78) set 100 rows. [Claude Code Grep](https://code.claude.com/docs/en/tools-reference#grep-tool-behavior) provides regex, file filters and offsets. Copy those controls while retaining Arany's literal default and bounded in-process execution to avoid reinterpreting existing queries or admitting a subprocess.

[regex's complexity contract](https://docs.rs/regex/latest/regex/#untrusted-input) guarantees `O(m × n)` for `is_match`, not repeated match iteration. With a bounded compiled pattern, one `is_match` per line stays linear in scanned content. The [builder](https://docs.rs/regex/latest/regex/struct.RegexBuilder.html#method.size_limit) supports explicit compilation bounds; no dependency change is required.

Rules and ownership: [planning](../PLANNING.md), [library](../../agents/arany.md), [security](../../agents/security.md#security-pass-triggers), [testing](../../agents/testing.md#user-visible-proof) and the [runtime architecture](../../docs/architecture/system-overview.md#2-runtime-architecture).

## Contract and scenarios

Owning spec: [Tools in-process reads](../../docs/specs/tools.md#in-process-reads), its [limits](../../docs/specs/tools.md#limits-and-failure-behavior) and hook narrowing. Replace only Search's 64-row contract; List stays at 64.

Proposed call: `{path, query, regex, include, case_sensitive, offset, limit}`. Keep `path` and `query` required. New fields accept omitted/null defaults: `regex: false`, `include: null`, `case_sensitive: true`, `offset: 0`, `limit: 100`. Strict native schemas expose the new fields as required nullable properties, following existing Read paging. Validate query at 1–512 UTF-8 bytes, include at 1–512 bytes when present, limit at 1–100 and offset at 0–33,554,432. Canonicalize defaults before intent hashing and hook comparison.

Regex is Rust regex syntax within one UTF-8 line; look-around/backreferences and invalid or over-limit patterns reject before scanning. Literal mode escapes the query before compiling with case control. Case-insensitive mode uses regex's Unicode folding; explicit regex inline flags retain Rust regex semantics. Include filters file paths relative to a directory `path`; for a named file it matches that file's basename. Use the [bounded glob grammar](../../docs/specs/tools.md#glob-file-discovery); authorization always precedes applying a filter. Do not read excluded files solely to filter them; preserve existing whole-project directory exclusions.

Order rows by Workspace-relative path in UTF-8 byte order, then one-based line number. Deduplicate overlapping selected roots. Offset skips matching **rows**, not bytes, lines or files. Keep `{entries, limit, may_be_truncated}` and add nullable `next_offset`: it equals the requested offset plus rows actually returned when another row exists, otherwise null. Detect one additional matching row without retaining skipped matches. Fit metadata and rows to the existing rendered result budget, dropping only trailing rows; resume at the first dropped row. If matches exist but no row fits, record Limit. Offsets past EOF return empty entries and null continuation. Pages are fresh bounded reads, not a filesystem snapshot across calls; replay uses each committed observation.

| Scenario | Required outcome and evidence owner |
|---|---|
| Existing literal call; regex/case/include combinations | Existing literals keep their meaning; matching returns correct paths/line numbers and bounded previews. Extend [Tool corpus](../../crates/arany/src/tools/tests.rs) and [private-worker corpus](../../crates/arany/src/tools/worker.rs). |
| 0, 100, 101 and several hundred matching lines, with shuffled directory insertion | Deterministic pages have no skipped or repeated rows on an unchanged tree; exact 100-row EOF has null continuation. The same corpus owns smaller byte-budget pages and past-EOF offsets. |
| Malformed regex/glob, compiled-size overflow, offset/limit overflow or unsupported syntax | Reject whole before content inspection; never fall back to literal matching or report a false empty success. Tool/decoder boundary tables own rejection. |
| `.env`, protected descendants, outside-grant paths, links and replaced Workspace | Preserve exclusion/denial and pinned-handle checks with canaries absent from results, journal, diagnostics and Provider requests. Extend the existing in-process refusal corpus and [session Tool journey](../../crates/arany-cli/tests/session_run/tools.rs). |
| Hook updates query, mode, case or include; valid narrowing; delegated update | Reject widening or unprovable updates; accept only the rule below for primary calls, reauthorize a new intent, and preserve delegated-update denial. Extend [permissions corpus](../../crates/arany/src/tools/permissions/tests.rs) and [Guard permission journeys](../../crates/arany-cli/tests/guard_runs/permissions.rs). |
| Read slot full, scan overflow, timeout, cancellation or journal reopen | Preserve bounded Limit/Cancelled observations and slot ownership until scan completion; closed-store replay never scans again. Tool, reducer and session journeys own these paths. |

## Chosen direction

Ponytail: compile once per call with the installed `regex` crate, use one boolean match per line and a bounded page accumulator. Keep this inside the existing filesystem family; extend the WorkerView inspection path too. Reuse the existing capability traversal, not a shell, new index or adapter trait. Share glob translation with Glob when both features are present; the first landed PR supplies the helper, and neither feature depends on the other's runtime availability. Permission-pattern semantics remain separate because their recursive/deny rules differ.

Hooks preserve byte-identical query, regex mode, include and case after default normalization. For the same path, a replacement page must lie wholly inside the original row window, using checked arithmetic. A subtree replacement is allowed only with both offsets zero and a no-larger limit; this preserves existing unpaged subtree narrowing without claiming that a filtered subtree has the same row indices. Re-evaluate pinned rules and grant as today. A different pattern is a new model proposal, never a purported proof of regex-language containment.

Tools owns normalized fields, matcher and traversal; Engine owns permission, ordering, budgets and cancellation; Journal/reducer owns correlated intent and observation; presentation consumes the ordinary output projection. The interface stays in-process and carries only typed fields and bounded JSON results. Update native/structured schemas, descriptions and catalog snapshots together. Changed persisted Search shape must version every enclosing stored payload that carries it and ship the applicable migration plus fixture; unsupported history follows [Execution recovery](../../docs/specs/execution.md#admission-persistence-and-recovery), without a compatibility reader.

Linux/macOS use identical matching and byte sorting independent of locale or case-insensitive host lookup. Keep existing filesystem adapters and native-admission requirements. Synthetic evidence and this plan do not establish named-host latency or native support.

## Security and resource model

Triggered pass: new parsing and filesystem scope filtering. Assets are admitted file contents, private history and bounded CPU/memory. Model-controlled patterns, offsets and repository paths are data. Authority remains authenticated settings intersected with the pinned parent/child grant; filters only narrow. Startup admission order, secrets, credentials, network and descendant authority do not change. Retain no-follow, mount, hard-link, special-file, protected-name, identity and expiry checks and the InProcess attestation boundary; ambient Engine privileges are not an OS sandbox claim.

Compile query and optional include separately with 128 KiB size, 32 KiB DFA cache and nesting 32, matching existing bounded-builder practice. The glob translation is at most 8 KiB with at most 16 brace alternatives; reject overflow before compilation. No persistent/global matcher cache. Retain the researched scan/content/depth bounds, 512-character previews, 64 KiB rendered results and 16 shared read slots. Hold at most 100 candidate rows plus one lookahead, and skip offsets without collecting them. Do not enlarge Tool argument or Event bounds for this feature. Preserve finite per-action runtime, Goal charging and Store aggregate quotas; scan overflow is Limit, never a successful partial scan disguised as EOF. Check cancellation/deadline between bounded units; timed-out blocking scans retain their slot until completion. Diagnostics use bounded classifications, not raw patterns or file contents.

## Risks and mitigations

- Paging loses rows when result fitting removes them → compute continuation from emitted rows and prove this with a reduced result budget.
- Repeated match iteration defeats the intended complexity guarantee → one `is_match` per line; bound both pattern compilation and scanned bytes.
- Hooks switch regex meaning or reinterpret offsets after narrowing → compare normalized matching fields and use the conservative window/subtree rules above.
- Private-worker output diverges from Workspace search → use the same matcher/page accumulator and test private edits before handover.

## Vertical slices

1. **One PR: search and page filtered regex matches through the existing read Tool.** Extend model schemas/structured decoding → typed admission and hook narrowing → committed ToolStarted → shared bounded Workspace/WorkerView matching and page fitting → attested ToolFinished → model/`show` output and closed-journal replay. Include Tools contract/scenario updates, stored-format migration evidence, catalog snapshots and the success, hostile-input, paging and cancellation cases above. Every advertised option works in this PR; the feature is usable without a new process Tool or parallel-call support.

## Exit criteria

- The named owners prove literal defaults, regex/include/case behavior, a hard 100-row ceiling, deterministic multi-page results and byte-budget-safe continuation.
- Invalid patterns, hostile paths, hook widening, capacity overflow and cancelled/timed-out scans retain bounded, honest outcomes; replay performs no I/O.
- Implementation passes formatting, Clippy, targeted Tool/decoder/reducer/session checks and final-head Linux/macOS CI. Keep unavailable live/native/performance evidence with its owner in [NEXT_STEPS](../../NEXT_STEPS.md).
- Tools becomes the accepted contract; remove the plan and update inbound links under the [planning lifecycle](../README.md#lifecycle).
