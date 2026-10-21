# Read-only Glob Tool

## Metadata

- Status: `Draft`
- Owner: Arany maintainers
- Last updated: `2026-10-21`

## Outcome and scope

Add a read-only, in-process `glob` Tool with `{pattern, path, limit}` to find regular files recursively by name, with a default and maximum of 100 entries in deterministic path order. It serves harness exploration without requiring a Guard command. This is one new read capability; it does not add content search, `.gitignore` discovery, modification-time sorting, directory results or parallel Tool calls.

## Research

Verified against this worktree on 2026-10-21.

| Finding | Source |
|---|---|
| Catalog and ToolCall currently expose List/Read/Search, with no Glob operation. The catalog pins their execution site; the site test expects precisely these three in-process Tools. | [crates/arany/src/tools/catalog.rs:88](../../crates/arany/src/tools/catalog.rs), [catalog.rs:224](../../crates/arany/src/tools/catalog.rs), [catalog.rs:564](../../crates/arany/src/tools/catalog.rs), [crates/arany/src/tools/types.rs:169](../../crates/arany/src/tools/types.rs) |
| `Glob` is already a peer permission spelling for bare List. `Read` covers the read family; path specs belong to Read, not bare Glob. Adding the native `glob` name must preserve List restrictions too. | [crates/arany/src/tools/permissions.rs:561](../../crates/arany/src/tools/permissions.rs), [permissions.rs:567](../../crates/arany/src/tools/permissions.rs), [permissions.rs:692](../../crates/arany/src/tools/permissions.rs), [Tools permission mapping](../../docs/specs/tools.md#permission-rules) |
| `regex` is already a direct workspace dependency at exactly 1.13.1. `rg -n 'globset' Cargo.lock` returns no matches (exit 1); no globset package is locked. | [crates/arany/Cargo.toml:27](../../crates/arany/Cargo.toml), [Cargo.toml:45](../../Cargo.toml), [Cargo.lock:2861](../../Cargo.lock); absence checked across [Cargo.lock](../../Cargo.lock) |
| Permissions already translate path globs into regex and cap compilation, but deliberately append descendant matching and case-fold restrictions. Those policy semantics must not become discovery semantics accidentally. | [crates/arany/src/tools/permissions.rs:713](../../crates/arany/src/tools/permissions.rs), [permissions.rs:797](../../crates/arany/src/tools/permissions.rs), [permissions.rs:805](../../crates/arany/src/tools/permissions.rs) |
| Existing collection reads file contents; a metadata-only walker must reuse capability checks without calling that content collector. Scanning already has finite entry/depth/file counts, and whole-project enumeration skips `target`, `node_modules`, `.venv` and `venv`. | [crates/arany/src/tools/fs.rs:647](../../crates/arany/src/tools/fs.rs), [fs.rs:682](../../crates/arany/src/tools/fs.rs), [fs.rs:726](../../crates/arany/src/tools/fs.rs), [fs.rs:741](../../crates/arany/src/tools/fs.rs), [fs.rs:769](../../crates/arany/src/tools/fs.rs), [crates/arany/src/tools/types.rs:26](../../crates/arany/src/tools/types.rs) |
| Protected-name validation rejects `.env`, protected case variants and `.env.*`; paths are at most 512 bytes. In-process reads share capacity, pinned handles, timeout and cancellation behavior. | [crates/arany/src/tools/types.rs:326](../../crates/arany/src/tools/types.rs), [types.rs:358](../../crates/arany/src/tools/types.rs), [types.rs:373](../../crates/arany/src/tools/types.rs), [crates/arany/src/tools.rs:1147](../../crates/arany/src/tools.rs), [tools.rs:1223](../../crates/arany/src/tools.rs) |
| Workers have their own closed Tool labels/grants and image inspection path, with WorkerRead rather than Workspace InProcess attestation. Glob must be explicitly mapped and narrowed there rather than inheriting authority by name. | [crates/arany/src/session/worker.rs:23](../../crates/arany/src/session/worker.rs), [worker.rs:150](../../crates/arany/src/session/worker.rs), [crates/arany/src/tools/worker.rs:429](../../crates/arany/src/tools/worker.rs), [worker.rs:1100](../../crates/arany/src/tools/worker.rs) |

Freeze check: [NEXT_STEPS.md:70](../../NEXT_STEPS.md#frozen-areas) freezes “new typed Tools other than the in-process read set.” A metadata-only, read-only in-process Glob fits that explicit exception; it adds no new Guard/process capability or exemption to a guarantee.

Peer source: [Claude Code Glob](https://code.claude.com/docs/en/tools-reference#glob-tool-behavior) supports recursive and brace patterns, a 100-file cap and modification-time sorting. [OpenCode Glob](https://github.com/anomalyco/opencode/blob/dev/packages/core/src/tool/plugin/glob.ts#L13-L23) exposes pattern/path/limit; [its shared default](https://github.com/anomalyco/opencode/blob/dev/packages/core/src/filesystem.ts#L62-L70) is 100. Copy the small call shape and cap; choose lexical path order rather than Claude Code's mtime order for deterministic observations without extra mutable ordering inputs. [regex](https://docs.rs/regex/latest/regex/#untrusted-input) supports bounded linear-time boolean matching; use it instead of adding globset.

Rules: [planning](../PLANNING.md), [library](../../agents/arany.md), [security pass](../../agents/security.md#security-pass-triggers), [testing](../../agents/testing.md#user-visible-proof) and [runtime architecture](../../docs/architecture/system-overview.md#2-runtime-architecture). The current [in-process read contract](../../docs/specs/tools.md#in-process-reads) owns the capability/attestation boundary.

## Contract and scenarios

Owning spec: [Tools](../../docs/specs/tools.md#in-process-reads), including permission mappings, hook narrowing, limits and scenarios. Extend the overview's existing read-set description without introducing a module/process seam. Record any new persisted enum/shape versions and recovery at the [Execution owner](../../docs/specs/execution.md#admission-persistence-and-recovery).

Call shape: required nonempty `pattern` of at most 512 UTF-8 bytes, nullable/omittable `path` defaulting to `.`, and nullable/omittable `limit` defaulting to 100. Limit is 1–100; larger or zero values reject rather than clamp. Native strict schemas require nullable path/limit properties; normalize defaults before intent hashing. Path names a directory inside the admitted Workspace/WorkerView; a named regular file fails as a non-directory. Match relative to that directory; return Workspace-relative paths.

Result: `{entries: [{path, bytes}], limit, may_be_truncated}` for regular files only. Sort unique paths by UTF-8 byte order before selecting the first limit entries. Set truncation only when an additional match exists or trailing matches are removed to fit the rendered result budget. Exactly 100 matches at limit 100 is complete. If matches exist but no entry fits, record Limit. No content bytes, hashes or edit preconditions are implied by metadata; use Read for a digest. No pagination state is added; callers narrow pattern/path when truncated.

| Scenario | Required outcome and evidence owner |
|---|---|
| `**/*.rs`, `src/**`, `*.{toml,md}`, zero matches, explicit smaller limit | Return the specified regular-file set with byte sizes and correct ordering/metadata. Extend [Tool corpus](../../crates/arany/src/tools/tests.rs) and shared filesystem boundary tables. |
| 99, 100 and 101 matches with randomized fixture insertion order; reduced result budget | Deterministic first entries and honest truncation; no false truncation at exact EOF or empty success when no entry fits. Same corpus; no timing-based assertions. |
| Protected names/descendants, hidden permitted names, link/mount/hard-link/special-file escapes, overlapping roots | Never disclose protected paths or widen the selected scope; hidden names otherwise follow admitted enumeration, not a new hidden-file bypass. Dedupe roots. Extend existing refusal/canary cases. |
| Invalid pattern, absolute/traversal path, size/scan/depth overflow, missing/non-directory path | Reject invalid input before traversal; scan overflow records Limit without pretending complete success; missing path uses existing Failed semantics, forbidden paths remain Denied. Tool and session journeys own these outcomes. |
| Read/Glob/native-name deny/ask, researcher/worker use and hook update | Apply all mappings and inherited restrictions; narrow only as specified below. Private-worker Glob sees only its admitted image, including acknowledged edits. Extend [permissions](../../crates/arany/src/tools/permissions/tests.rs), [worker](../../crates/arany/src/tools/worker.rs) and [Guard permission journeys](../../crates/arany-cli/tests/guard_runs/permissions.rs). |
| Full read capacity, cancelled/timed-out scan, replaced Workspace, closed journal replay | Keep read-slot ownership and honest Limit/Cancelled or pinned-input failures; replay renders recorded metadata without rereading, checking mtimes or dispatching hooks. Extend [session Tool journey](../../crates/arany-cli/tests/session_run/tools.rs) and [reducer corpus](../../crates/arany/src/session/reducer/tests.rs). |

## Chosen direction

Ponytail: extend the existing closed filesystem Tool family and in-process site. Reuse handle-relative traversal and result fitting, adding only a bounded metadata walk and glob-to-regex translation. Keep permission-pattern translation's policy behavior separate. [Search include filtering](../search-regex/README.md#chosen-direction) uses the same discovery matcher when present; whichever feature lands first supplies that private helper. Neither feature requires the other's catalog entry. No globset, ripgrep process, filesystem index, plugin trait or new crate.

The discovery grammar is deliberately closed:

- `/` separates components; match the entire relative path with `\A` and `\z`. A separator-free pattern matches only an immediate basename; use `**/` to include deeper paths.
- `*` matches zero or more non-separator characters, `?` one Unicode non-separator character, and `**` is permitted only as a complete component. `**/` includes zero directories; terminal `**` matches the remaining path.
- A single-level `{a,b}` group supplies literal/wildcard alternatives within one component. Permit at most 16 alternatives in total across groups; compile grouping directly, without Cartesian expansion. Empty alternatives, nesting and separators inside braces reject.
- Escape regex metacharacters in literals. Reject bracket classes, backslash escapes, negation, absolute/home prefixes, controls, empty components, `.`/`..`, malformed braces and unsupported `**` placement. Do not silently reinterpret invalid syntax as a literal or command.

Bound translation to 8 KiB before regex compilation; set compiled size to 128 KiB, DFA cache to 32 KiB and nesting to 32. Compile once per call and use `is_match` per candidate. File matching is case-sensitive independent of host locale; protected-name checks retain their conservative case handling.

Map native `glob` into Read permissions and peer `Glob`, retaining bare peer Glob's existing coverage of List. Native lowercase `glob` rules name this operation only. Keep path specifiers on Read, update hook subjects/catalog matchers and include Glob only in child catalogs/grants already narrowed from the parent. Hook `updatedInput` may retain the normalized pattern, select an equal/subtree path and lower the limit; pattern changes and raised limits deny. Reauthorize the new immutable intent; delegated updates remain denied as in the existing contract.

Tools owns matcher, bounds and read metadata; Engine owns permission/admission and durable ordering; the reducer owns correlated state and accepted execution site. Add Glob to the existing WorkerTool/grant inventory with a finite updated variant-count bound and explicit read-path checks, and to WorkerView inspection. Canonical state stays in Journal; temporary matches die with the call. Extend all schemas, facts-table/site checks, renderer/catalog snapshots and recorded enum payload versions together. Any stored format change ships its migration and fixture; no compatibility reader. Linux/macOS share sorting and matching, retain current capability adapters and require native evidence before support claims.

## Security and resource model

Triggered pass: a new filesystem discovery Tool. Assets are admitted file names/sizes, private history and bounded scan resources; model patterns and repository metadata are attacker-controlled. Authenticated settings and a pinned typed grant determine authority before existence checks. A pattern never grants access. Preserve startup ordering, parent/child intersections, path/expiry/digest checks, no-follow directory handles, mount/link/special-file refusals and InProcess attestation. Glob writes nothing, starts no process and uses no credentials or network; separately configured hooks still run only in their existing Guard boundary.

Do not read file contents merely to obtain sizes, or call the content snapshot collector. Preserve existing whole-project directory exclusions. Enforce 8,192 scanned directory entries (including excluded entries), 2,048 candidate regular files, depth 16 and 512-byte admitted paths. Overflow records Limit, not a partial success; result truncation is only for match count/rendered bytes. A file's byte size does not require admitting its content and is not subject to a new read. Retain 64 KiB rendered results, 16 shared read slots, finite intent runtime and Goal/Store accounting. Retain at most the bounded candidate metadata and 100 output entries plus lookahead; no persistent cache or retry. Cancellation/timeout retains a blocking scan's slot until the bounded walk ends. Secret canaries and hostile controls must not reach output/errors/diagnostics or Provider context. This in-process capability boundary is not kernel isolation against hostile same-user/root mutation.

## Risks and mitigations

- Policy globs accidentally change discovery or grant semantics → share only the discovery matcher with Search, preserve permission matching and test both mappings.
- Metadata walking silently reads content or follows links → use admitted handles and metadata-only operations; test over-1-MiB files and hostile objects.
- Enumeration order leaks into capped output → sort/dedupe the bounded candidate set before selecting entries, including overlapping roots.
- New child enum grants broaden access → explicit variant mapping, parent intersection and private-image tests; never infer authority from the Tool name.

## Vertical slices

1. **One PR: discover admitted files by glob through a complete read-only Tool call.** Wire native/structured catalog and schemas → Read/Glob permission and hook handling → committed ToolStarted → bounded Workspace metadata matching under the existing read-capacity owner, or admitted WorkerView image inspection → ToolFinished with the existing InProcess or WorkerRead attestation respectively → model/`show` output and strict closed-journal replay. Include the Tools contract/scenarios, overview read-set update, enum/version migration fixture, catalog/site snapshots and the success/refusal/truncation/cancellation tests above. This is usable by itself and stays within the freeze's in-process read exception.

## Exit criteria

- The owners above prove the closed grammar, default/hard 100-entry cap, deterministic path order, byte-budget fitting and metadata-only behavior.
- Read/Glob/native and child rules, hook narrowing, protected-name exclusions, filesystem refusals and bounded cancellation/overflow retain their guarantees; replay never touches the filesystem.
- Implementation passes formatting, Clippy, targeted Tool/permission/worker/reducer/session checks and final-head Linux/macOS CI. Unavailable native/live evidence remains owner-run in [NEXT_STEPS](../../NEXT_STEPS.md); no new support claim is made here.
- Accepted behavior moves to Tools and the existing architecture owner; remove the plan and inbound links under the [planning lifecycle](../README.md#lifecycle).
