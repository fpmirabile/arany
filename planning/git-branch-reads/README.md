# Inspect branches and compare their work

## Metadata

- Status: `In progress` (S2 remains).
- Owner: `fpmirabile`; the implementation lead owns slice delivery and evidence.
- Last updated: `2026-10-22`.
- Next action: implement S2 on S1; this plan does not depend on host Git publication.

## Outcome and remaining scope

The primary can discover local and remote-tracking tips through
[branch discovery](../../docs/specs/tools.md#branch-discovery). S2 completes this
feature with bounded range history and the existing endpoint diff. The
[decision register](../../docs/research/next-step-decision-register.md#d-23--branch-reads-use-git-semantics-with-inert-tracking-metadata)
owns the delivered discovery rationale; Tools owns its behavior and evidence.
[GitHub PR reads](../github-pr-reads/README.md) supplies remote review/check
information separately.

Keep the offline neutralized Guard view: no fetch, network, arbitrary shell/argv,
new Git layout support or change to Git writes. The ordinary-command missing-`.git`
hint is separate work.

## Research and accepted decisions

Research and Plan gates passed on 2026-10-22 against checkout
`8d4281e3768142574b60e7051e9015969d29dc2e`. The [Git adapter](../../crates/arany/src/tools/git.rs)
already validates revisions, constructs fixed argv, filters results and shares
one action deadline; its log input rejects ranges. Its typed diff already
compares explicit endpoints. [Git log](https://git-scm.com/docs/git-log) defines
`A..B` as commits reachable from B but not A;
[Git diff](https://git-scm.com/docs/git-diff) defines `diff A..B` as `diff A B`,
not the merge-base patch `diff A...B`.

Owner decisions, Decided 2026-10-22:

- Allow exactly one explicit `base..head` in `git_log.revision`, with two
  independently validated existing revisions. No open endpoints, three-dot,
  revision sets, options, `:`, `@{` or raw argv. Each endpoint is at most 128
  bytes, the combined expression at most 258 bytes.
- Keep `git_diff {base,head,staged:false}` as the typed spelling of endpoint
  `git diff base..head`; no new diff operation.
- Resolve comparison endpoints once to commit OIDs before dependent reads and
  include them in the committed observation. At most four invocations share
  the existing deadline; keep 50 returned log commits and the existing
  1,000-commit filtered scan ceiling.
- Keep existing primary-only read grants, permission narrowing, plan scope,
  disclosure filters, Goal/Store reservations and Guard cleanup. No new crate,
  trait, cache, dependency or refresh process. Tools owns validation/OID
  resolution/JSON; Engine owns admission and journal ordering; Guard owns
  execution; reducer/Store own non-executing replay. Linux and macOS use their
  existing pinned Git adapters; macOS write unavailability must not disable reads.

If implementation contradicts a finding or decision, stop and return to Research.

## Remaining vertical slice

**S2 — Explain a branch with bounded range history and endpoint diff (one PR,
on S1).** Extend log range validation and Provider descriptions → policy/path
admission → committed intent → OID resolution and fixed guarded log/diff →
filtered journaled result → model continuation and consistent CLI views.
Reuse the existing diff operation and pin its two-dot equivalence.

| Scenario | Required observation and evidence owner |
|---|---|
| Divergent branches | After branch discovery, range log excludes base-only ancestors and endpoint diff compares the two trees, including where three-dot differs; an empty range succeeds. [Git corpus](../../crates/arany/src/tools/git.rs), [Tool/schema tests](../../crates/arany/src/tools/tests.rs), [Provider snapshots](../../crates/arany/src/provider/snapshots), [Linux journey](../../crates/arany-cli/tests/session_run/tools.rs) |
| Missing and hostile data | Missing endpoints/objects fail without fetch; protected-only history, denied paths and diagnostics never leak to Provider input or journal. Git, [permission tests](../../crates/arany/src/tools/permissions/tests.rs), Linux and [macOS journey](../../crates/arany-cli/tests/session_run/tools_macos_git.rs) |
| Bounds and recovery | Malformed/over-limit ranges reject before dispatch; overflow truncates complete records honestly; one deadline bounds work; changed pin/layout fails closed; cancellation confirms Guard cleanup; unknown completion is Uncertain and stops continuation; closed-store replay restores OIDs/results without Git or config reads. Product, reducer and Store owners |

## Security and delivery gates

The [security pass](../../agents/security.md#security-pass-triggers) applies to
new Tool input and disclosure. Repository refs, objects, configuration and
subjects are attacker-controlled data; only admitted user grants confer authority.
Fixed argv, unchanged private Git configuration/environment, offline Guard
execution, child exclusion and existing output bounds remain mandatory. No new
queue, automatic retry or authority is added. Exclusion filters retain their
existing non-sandbox limitation. Range syntax must not imply merge-base semantics.

Update [Tools](../../docs/specs/tools.md#typed-git-reads) and any changed persisted
shapes in [Execution](../../docs/specs/execution.md#admission-persistence-and-recovery).
A stored-version change ships the [migration contract](../../docs/specs/execution.md#journal-migration)'s
step and canonical fixture, without compatibility readers. Scenarios use the
existing [ObservationBundle](../../agents/testing.md#user-visible-proof) owners,
including reopened Events and reduced views. Local checks follow the coder brief;
full tests run in CI. Native/live evidence is not claimed; missing procedures
belong once in NEXT_STEPS. Close/remove this plan and its index row with S2 under
the [planning lifecycle](../README.md#lifecycle).
