# Inspect branches and compare their work

## Metadata

- Status: `Ready` (Research and Plan gates passed; implementation not started).
- Owner: `fpmirabile`; the implementation lead owns slice delivery and evidence.
- Last updated: `2026-10-22`.
- Next action: implement S1 on current main; this plan does not depend on host Git publication.

## Outcome and scope

One feature: the primary can discover local and remote-tracking branches and explain their differences using typed Git reads. This supplies the local evidence needed to assess outstanding work. [GitHub PR reads](../github-pr-reads/README.md) supplies remote review/check information separately.

Keep the existing offline, neutralized Guard view. Remote-tracking refs are locally stored observations, not live remote branches. No fetch, network, arbitrary shell/argv, new Git layout support or change to Git writes belongs here. The ordinary-command missing-`.git` hint is separate work.

## Research

Checked against checkout `8d4281e3768142574b60e7051e9015969d29dc2e` and primary sources on 2026-10-22:

| Finding | Source and consequence |
|---|---|
| Reads already validate revisions, construct fixed argv, run a pinned Git in a private view, filter output and share one deadline. `git_log` rejects ranges; `git_diff {base,head}` already compares endpoints. | [Typed Git reads](../../docs/specs/tools.md#typed-git-reads); [Git adapter](../../crates/arany/src/tools/git.rs), especially `valid`, `invocations`, `execute` and `log_output`. Extend these paths. |
| The checkout already offers `git_branch {name:null,start_point:null}` with local name/OID/current rows. Its read form is available without write authority, including macOS and plan mode. | [Typed Git writes, operations/availability](../../docs/specs/tools.md#typed-git-writes), [catalog](../../crates/arany/src/tools/catalog.rs). Extend this read form rather than add a competing listing Tool. |
| Claude's shipped command uses ordinary `git branch --show-current` and `git diff HEAD`. Codex implements ordinary Git discovery/comparison with `for-each-ref`, `rev-list --count` and `rev-list --left-right --count`. | Pinned [Claude command](https://github.com/anthropics/claude-code/blob/5ed799e5cc56bea9efff57398ca43e31d82784ff/plugins/commit-commands/commands/commit-push-pr.md), [Codex discovery](https://github.com/openai/codex/blob/86e17e98c7f901277aeeb4649cf1b01462b082f2/codex-rs/git-utils/src/info.rs), [Codex comparison](https://github.com/openai/codex/blob/86e17e98c7f901277aeeb4649cf1b01462b082f2/codex-rs/git-utils/src/branch.rs). These establish standard Git semantics, not an upstream typed model API or a promise that every prompt runs the same commands. |
| `log A..B` selects commits reachable from B but not A. `diff A..B` equals `diff A B`; it is not the merge-base comparison `diff A...B`. | Official [log](https://git-scm.com/docs/git-log), [diff](https://git-scm.com/docs/git-diff) and [count](https://git-scm.com/docs/git-rev-list) contracts. Preserve this distinction in schema descriptions and observations. |
| Branch OIDs/subjects come from refs/objects; upstream association comes from branch remote/merge and remote fetch configuration, not branch-name similarity. | Official [ref fields](https://git-scm.com/docs/git-for-each-ref) and [configuration](https://git-scm.com/docs/git-config). A neutralized view cannot simply request `%(upstream)` and expect the real association. |

Research gate passed: the current capability, missing operations, upstream limitation and peer semantics have source owners. Plan gate passed: the decisions below leave no material question. If implementation contradicts a finding, return to Research before continuing.

## Chosen direction

All decisions below are **Decided 2026-10-22**.

| Decision | Basis |
|---|---|
| Extend the existing branch read form with nullable `scope`, `base` and `max_count`. For a read, null scope means `local`, otherwise `local`, `remote` or `all`; null count means 256, otherwise 1–256. A null base omits comparison. Creation requires all three new fields null and keeps its existing meaning. | Peer parity: [Git branch scopes](https://git-scm.com/docs/git-branch); existing Arany overload and 256-row bound; Ponytail reuse. |
| Return sorted fully qualified ref names, local/remote kind, tip OID, current flag, nullable tip subject, upstream status and nullable ahead/behind counts, plus resolved comparison base/OID and `truncated`. Remote symbolic aliases such as `origin/HEAD` carry their target and are not counted as independent branches. | Peer parity: `for-each-ref`; explicit identities prevent local/remote short-name collisions. |
| Parse upstream metadata as inert data, never apply repository configuration to Git. Report unknown/unsupported associations explicitly instead of guessing `origin/<name>`. | Safe AI cage and existing neutralization, narrowing ordinary Git configuration behavior; upstream source above. |
| Allow exactly one explicit `base..head` expression in `git_log.revision`, with two independently validated existing revisions. Keep `git_diff {base,head,staged:false}` as the typed spelling of `git diff base..head`; no additional diff operation is needed. | Peer parity: Git log/diff semantics above. No open-ended endpoints, three-dot input, revision sets, options, `:`, `@{` or raw argv. |
| Resolve comparison endpoints once to commit OIDs before dependent reads; include those OIDs in the committed observation. Use fixed `rev-list --left-right --count base_oid...tip_oid`: left is behind, right is ahead. | Peer parity: pinned Codex comparison; journal-first reproducibility and honest uncertainty. |
| Keep the current primary-only read grant, permission narrowing, plan scope and result filtering. No new crate, trait, cache, background refresh or dependency. | [Tools](../../docs/specs/tools.md#typed-git-reads), [runtime ownership](../../docs/architecture/system-overview.md#2-runtime-architecture); owner scope. |

**Upstream extraction.** Extend the existing no-follow, at-most-64-KiB `.git/config` inspection. Only read direct `branch.<name>.remote`, `branch.<name>.merge` and the matching `remote.<name>.fetch` as descriptive values. Recognize ordinary Git-written quoted subsection names and plain/quoted scalar values in the existing admitted ref alphabet; no escapes, continuations, includes, environment expansion or external files. Support remote `.` and one unambiguous positive literal or single-wildcard fetch mapping, with optional `+`. Duplicate/ambiguous keys, negative or unsupported mappings and unsupported syntax produce `upstream: null` with `upstream_status: unsupported`. Any include/includeIf declaration makes upstream extraction unsupported, since an unread file could override direct values. Missing direct metadata is `unavailable`, a resolved missing ref is `gone`, and a present target is `present`; absence never proves that user-global Git has no tracking configuration. Remote rows have no branch upstream (`not_applicable`). Never load URLs, helpers or executable settings, pass these values as Git configuration, use them for authorization or initiate a fetch. This is an explicit metadata-reader change to the current extension-only config inspection, not permission to honor repository config. It preserves the guarantee while deliberately reporting less than Git when includes or unusual refspecs are required.

**Bounds and fidelity.** Reuse `for-each-ref` with fixed format, `--sort=refname`, selected `refs/heads/` / `refs/remotes/` namespaces and at most 257 rows (one lookahead). Admit at most 256 rows for enrichment; never launch a process per unbounded ref. At most one enumeration, one base resolution and two enrichment invocations per returned row (subject visibility and comparison), all sequential under the same 20-second action deadline and existing Guard resource profile. Remote refs can be stale; no freshness claim follows from a successful read. A concurrent ref update does not change a captured row OID; a vanished object fails explicitly. This is not an atomic repository snapshot.

Keep 256 KiB stdout/4 KiB stderr per invocation, 1 KiB filtered failure diagnostics, 256-character subjects, the existing final Tool-result budget and 50 returned log commits. Discard each enrichment capture after projecting its row; retain only bounded rows and the current capture. Each range endpoint remains at most 128 bytes (258 bytes for the combined expression). Reject invalid/over-limit inputs before execution; complete bounded rows may return `truncated`, never partial records or invented counts. Reuse the existing 1,000-commit ceiling for filtered log scans. Resolve up to two range endpoints once, then run the existing bounded log/diff path (at most four invocations under one deadline); do not turn the two-dot expression into a general revision parser.

Tip subjects must pass the existing history/path disclosure filter; a tip with no visible-path evidence yields null, never the subject of an older visible commit. Graph counts describe whole-history ancestry, not permitted-file change counts. Under a restrictive Read path policy suppress subject/count enrichment unless its disclosure can be proved; mark withheld fields, never substitute zero. Keep mandatory secret-path exclusions for subject evidence, logs, diffs and diagnostics. Shallow history reports `history_complete:false`; missing objects fail without lazy fetch, and counts are explicitly limited to available history. Detached/unborn HEAD does not prevent enumeration; an explicitly requested unresolved base fails, while an empty repository with no base returns an empty list.

**Ownership and platforms.** Tools owns validation, OID resolution, private-view metadata and bounded JSON translation; Engine owns admission, immutable intent/verdict/observation ordering and Goal reservations; Guard owns execution/teardown; reducer/Store own durable facts and non-executing replay. Provider schemas and CLI projections consume semantic fields, not Git output as policy. This extends the existing in-process Tool interface and native Guard process boundary. Use Linux's pinned Git and macOS's pinned Command Line Tools Git; macOS write unavailability does not disable these reads. No new native adapter or support claim.

## Contract and scenarios

Update [Tools: typed Git reads](../../docs/specs/tools.md#typed-git-reads), the branch read cross-reference, limits and acceptance scenarios as slices land. [Execution](../../docs/specs/execution.md#admission-persistence-and-recovery) owns new persisted shapes and replay validation. Existing evidence owners are [Git adapter tests](../../crates/arany/src/tools/git.rs), [schema/Tool tests](../../crates/arany/src/tools/tests.rs), [permission tests](../../crates/arany/src/tools/permissions/tests.rs), [Provider snapshots](../../crates/arany/src/provider/snapshots), [Linux product journey](../../crates/arany-cli/tests/session_run/tools.rs) and [macOS Git journey](../../crates/arany-cli/tests/session_run/tools_macos_git.rs).

| Scenario | Required observation and owner |
|---|---|
| Nine divergent branches, packed and loose refs, local/remote names colliding | One listing exposes exact names/OIDs, subjects, upstreams and correct behind/ahead against the captured base; then log/diff explain one branch. Git and product journey owners. |
| Range semantics | Exclude base-only ancestors from log; compare endpoint trees for diff, including diverged history where three-dot would differ. Empty range is successful. Schema, Git and Provider owners. |
| Missing/stale/shallow data | Gone upstream, missing base, absent objects, detached/unborn HEAD and symbolic remote aliases remain distinguishable; no fetch occurs. Git and native journey owners. |
| Hostile configuration and disclosure | Include/helper/hook/pager canaries never execute; unsupported upstream syntax is labeled; protected-only tip subjects and denied paths never leak through rows, diagnostics, Provider input or journal. Git, permission and product owners. |
| Bounds, races, cancellation and replay | Excess rows/results truncate honestly; malformed ranges reject; one deadline bounds all enrichment. Changed pin/layout fails closed. Cancellation confirms Guard cleanup; unknown completion is Uncertain and stops continuation. Closed-store replay restores committed rows/OIDs without running Git or rereading config. Product, reducer and Store owners. |

## Security and resource model

The [security pass](../../agents/security.md#security-pass-triggers) is triggered by new Tool inputs, config metadata parsing and disclosure. Assets are repository contents, protected paths and journal integrity; refs, objects, configuration and subjects are attacker-controlled data. Only admitted user grants confer authority. Fixed argv, unchanged private Git config/environment, offline Guard enforcement and child exclusion remain mandatory. No hook, ref, upstream association or Tool result can widen a grant. Reserve normal Goal/Store headroom before dispatch; no new queue, cache or automatic retry. A lost receipt is unknown, not an empty result. Exclusion filters retain their existing non-sandbox limitation.

## Risks and mitigations

- Upstream or ahead/behind could be plausible but wrong → explicit provenance/status, pinned endpoints and tests with non-default tracking and diverged graphs.
- Ref scans could multiply work or expose excluded subjects → bounded enumeration/enrichment, one deadline and shared disclosure filters before persistence.
- Two-dot could be confused with a PR's merge-base patch → schema and output describe endpoint semantics; GitHub metadata never changes them implicitly.

## Vertical slices

1. **S1 — Discover and compare local/remote-tracking branch tips (one PR).** Extend the existing branch read schema/catalog → grant and permission classification → immutable Engine intent and journal → neutralized Guard enumeration/upstream extraction/counts → bounded committed observation → model and terminal/exec/show. Deliver the listing, disclosure, hostile-config, overflow and replay scenarios above, including macOS read availability. Update Tools and affected Execution shapes in the same PR. Alone this lets the primary identify outstanding branches and their divergence.
2. **S2 — Explain a branch with bounded range history and endpoint diff (one PR, on S1).** Extend log range validation and Provider descriptions → policy/path admission → committed intent → OID resolution and fixed guarded log/diff → filtered journaled result → model continuation and consistent CLI views. Reuse the existing diff operation, pin its two-dot equivalence, and deliver the range, missing-object, cancellation and closed-replay scenarios. Update Tools and affected persisted shapes. Alone this completes branch assessment without shell Git.

## Exit criteria

- Both slices prove their success and meaningful failure paths through the named owners and [ObservationBundle](../../agents/testing.md#user-visible-proof), including closed-store Events and reduced views. A forged receipt, range or privilege change is rejected.
- A stored version change ships the existing [migration contract's](../../docs/specs/execution.md#journal-migration) required step and fixture, with no compatibility reader.
- Implementation runs targeted formatting, Clippy and relevant tests, then full CI. This plan update uses prose/link/diff checks only. Native Linux/macOS and live dogfood evidence are not claimed; unavailable checks get one owner/procedure in NEXT_STEPS when implementation lands.
- Delivered behavior moves to its owning specs; close/remove the plan and index row under the [planning lifecycle](../README.md#lifecycle).
