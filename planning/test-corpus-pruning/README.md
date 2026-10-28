# Test corpus pruning

## Metadata

- Status: `Ready`
- Owner: harness maintainers
- Last updated: 2026-10-26

## Outcome and scope

One observable result: each oversized corpus inventoried below moves to focused evidence owners, except the explicitly retained journey roots and migration corpus. The harness responsibility served is reviewable evidence for the [non-negotiable guarantees](../../AGENTS.md#non-negotiable-guarantees). Placement and pruning follow [Corpus placement](../../agents/testing.md#corpus-placement) and [Tests worth keeping](../../agents/testing.md#tests-worth-keeping); the latter owns the moved-block exception and deletion safeguards.

Excluded: new tests, behavior changes, fixture or Provider-double changes. `test_support/worker_teams.rs` is shared scenario support, not a corpus to prune; `guard_runs/worker.rs` is below the split signal. Shared-workspace-editors owns their protocol changes; the Gates section below pins the surviving-corpus handoff. Process/macOS modules with self-spawned filter paths (`cli/attached/run.rs::tests`, `session_run::active_terminal*`, `tools_macos`) retain those paths; the other focused Run tests live in [run/tests.rs](../../crates/arany-cli/src/cli/attached/run/tests.rs).

## Research

Inventory at `dfba8416`, measured with `wc -l` and `grep -cE '#\[(tokio::)?test'` over the linked sources. These count test attributes, not expanded or executed cases; execution counts are collected per slice.

| Corpus | Lines | Test attributes | Delivery or exclusion |
|---|---|---|---|
| [session/reducer/tests.rs](../../crates/arany/src/session/reducer/tests.rs) | 11,563 | 110 | T1a–T1c: reducer phases and families |
| [engine_run/goals.rs](../../crates/arany/tests/engine_run/goals.rs) | 10,042 | 116 | T2a–T2b: Goal scenario families; re-inventory at T2a start |
| [session/reducer/worker_tests.rs](../../crates/arany/src/session/reducer/worker_tests.rs) | 6,203 | 61 | T3a: surviving editor lease, stop and verification evidence after shared-workspace-editors S2b2 |
| [guard_runs/permissions.rs](../../crates/arany-cli/tests/guard_runs/permissions.rs) | 5,828 | 81 | T4a: modes, rules, hooks and host evidence |
| [tools/tests.rs](../../crates/arany/src/tools/tests.rs) | 5,723 | 62 | T3b: Tool families and cross-family catalog cases |
| [session_run/setup.rs](../../crates/arany-cli/tests/session_run/setup.rs) | 4,964 | 25 | T4b: [setup flow evidence](../../docs/specs/terminal.md#setup-and-account-choices) |
| [terminal/view/tests.rs](../../crates/arany-cli/src/terminal/view/tests.rs) | 4,810 | 50 | T4c: [Transcript evidence](../../docs/specs/terminal.md#interruption-suspension-and-restoration) |
| [engine_run.rs](../../crates/arany/tests/engine_run.rs) | 6,507 | 50 | T5a: in-process Engine scenario families |
| [session_run/slash_completion.rs](../../crates/arany-cli/tests/session_run/slash_completion.rs) | 4,592 | 12 | T5b: completion, pickers and paste journeys |
| [acp/in_process.rs](../../crates/arany-cli/tests/acp/in_process.rs) | 4,591 | 38 | T5c: transport, lifecycle and projection cases |
| [session_run.rs](../../crates/arany-cli/tests/session_run.rs) | 4,091 | 24 | Excluded: central Session process journeys and shared process fixture owner under Corpus placement; add new cases in submodules |
| [guard_runs.rs](../../crates/arany-cli/tests/guard_runs.rs) | 3,854 | 27 | Excluded: central Guard-launching journeys and shared native fixture owner under Corpus placement; add new cases in submodules |
| [serve/in_process.rs](../../crates/arany-cli/tests/serve/in_process.rs) | 3,845 | 34 | T5d: transport, lifecycle and provisional-display cases |
| [session_run/tools.rs](../../crates/arany-cli/tests/session_run/tools.rs) | 3,624 | 14 | T5e: process Tool journeys in child modules |
| [session/events/tests.rs](../../crates/arany/src/session/events/tests.rs) | 3,232 | 37 | T6a: Event family validation |
| [store/migrate/tests.rs](../../crates/arany/src/store/migrate/tests.rs) | 3,040 | 38 | Excluded: rule-5 migration and archive/recovery evidence, never pruned; new coverage goes beside a focused migration child |

[Codex's core suite](https://github.com/openai/codex/tree/main/codex-rs/core/tests/suite) groups integration evidence by feature, including approvals and task cancellation. This plan follows that locality while retaining Arany's central process journeys and migration evidence.

## Contract and scenarios

No observable behavior changes. A spec scenario linking a moved test gets its evidence link updated in the same PR ([spec workflow](../../docs/specs/README.md#maintain-a-contract)). Pruning decisions and surviving owners follow Tests worth keeping; a move that changes an assertion is a separate behavior change.

- Evidence per slice: old and new nonzero `cargo test` counts, with old count equal to new owners' count plus deleted count; the deletion inventory and separate deletion commit required by Tests worth keeping; `git diff --color-moved=dimmed-zebra` for moved blocks.
- Rejection: losing a materially different process, platform, durability or security lane, weakening an assertion, changing fixture behavior, or leaving an evidence/filter link unresolved.

## Chosen direction

- Simplest sufficient solution: move existing cases into existing module families or focused test children; no framework, macro or new fixture abstraction. Reuse existing support without copying its knowledge.
- No Linux/macOS behavior changes. Preserve platform gates and native/process evidence paths.
- No production ownership or interface change.

## Security and resource model

None change. Review every deletion against the [security coverage categories](../../agents/testing.md#evidence-lanes) and the same-lane surviving-owner requirement in Tests worth keeping. Rule-5 migration evidence is outside pruning scope.

## Risks and mitigations

- The only owner of a failure path is mistaken for a duplicate → compare its claim and lane with the named surviving owner before deletion.
- Moves conflict with active plans → obey the gates below and re-inventory after overlapping merges, preserving assertions.
- A moved test stops matching a CI or native filter → search `.github/` and `NEXT_STEPS.md` for its name, update references in the same PR, and demonstrate nonzero old/new execution counts.

## Vertical slices

Each row is one PR under [slice delivery](../PLANNING.md#slice-delivery), including its current exclusions for pure moves and listed deletions. Tests worth keeping still requires the deletion inventory and surviving evidence owners. Every destination follows Corpus placement and stays below its split signal; retain parent cross-child cases only. Re-inventory the row's test names, assertions and filters before moving them.

| ID | Complete move inventory and gate | Non-moved-line ceiling |
|---|---|---|
| T1a | Reducer Goal recovery, budget and plan cases from `reducer/tests.rs` to focused tests beside `reducer/goal` and its phase children. | ≤300 |
| T1b | Reducer Tool lifecycle, permission and hook cases to `reducer/tool` phase tests; after T1a and context-budgets S3b if it touches these owners. | ≤300 |
| T1c | Remaining Run, Agent, Session and rewind cases to their reducer family tests; cross-child cases remain in the parent; after T1b. | ≤300 |
| T2a | Re-inventory `engine_run/goals.rs`; move budget and plan-contract cases to `engine_run/budgets` and `engine_run/plan_contract` test children. | ≤300 |
| T2b | Move team, steering and recovery cases to `engine_run/{team,steering,recovery}` test children; retain only cross-family Goal cases; after T2a. | ≤300 |
| T3a | Re-inventory surviving `reducer/worker_tests.rs` after shared-workspace-editors S2b2; move lease, stop and verification cases beside their reducer children. Shared fixtures and `guard_runs/worker.rs` stay unchanged. Optional shared-editor S0b may deliver this move before cutover under the Gates exception. | ≤300 |
| T3b | Move `tools/tests.rs` cases beside `tools/{fs,process,git,mcp,permissions}` and catalog/type owners; only cross-family cases stay in the parent; after shared-workspace-editors S2b2. | ≤300 |
| T4a | Move `guard_runs/permissions.rs` into `permissions/{modes,rules,hooks,host}.rs`, preserving all process/native gates. | ≤300 |
| T4b | Move `session_run/setup.rs` into `setup/{native,chatgpt,custom}.rs`, leaving cross-flow journeys in the parent; preserve the [setup flow evidence](../../docs/specs/terminal.md#setup-and-account-choices). | ≤300 |
| T4c | Move `terminal/view/tests.rs` into view-region test children; retain cross-region frame cases in the parent and preserve [Transcript continuity](../../docs/specs/terminal.md#interruption-suspension-and-restoration). | ≤300 |
| T5a | Move `engine_run.rs` compaction, persistence, delegation and Tool-step cases into scenario-family submodules, preserving shared support and the integration target. | ≤300 |
| T5b | Move `session_run/slash_completion.rs` into completion, picker and paste journey submodules; preserve self-spawn filters and process support. | ≤300 |
| T5c | Move `acp/in_process.rs` transport, lifecycle and Tool projection cases to focused ACP test children; after display-facts D2. | ≤300 |
| T5d | Move `serve/in_process.rs` transport, lifecycle and provisional-display cases to focused serve test children, retaining existing `goal_controls` and `history_tests`; after display-facts D1. | ≤300 |
| T5e | Move `session_run/tools.rs` read/replay, MCP, permission and native containment journeys to Tool scenario children; preserve `native_exec` and `inline_approval` filter paths. | ≤300 |
| T6a | Move `session/events/tests.rs` Goal, defaults/account, subscription-compaction, Provider-call, child-grant and payload-version cases beside their Event family owners; retain only cross-family cases; after shared-workspace-editors S2b2. | ≤300 |

## Exit criteria

- The tracked test-file inventory below lists only the excluded `session_run.rs`, `guard_runs.rs` and `store/migrate/tests.rs`, or the explicitly retained platform/process modules. Every other inventoried oversized corpus has its completed slice owner.

  ```sh
  git ls-files 'crates/*/tests/**.rs' 'crates/**/tests.rs' 'crates/**/*_tests.rs' | xargs wc -l | awk '$1>3000 && $2!="total"'
  ```

- Every slice's test-count equation, deletion inventory and moved-block review are in its PR; all spec scenario links and CI/native filters resolve.
- Close under the [planning lifecycle](../README.md#lifecycle) when all listed slices and exit criteria are complete.

## Gates

The remaining worker/Tool/Event moves T3a, T3b and T6a open after shared-workspace-editors **S2b2** ([worker vocabulary retirement](../../docs/specs/execution.md#journal-migration)), without waiting for its private-target/native retirement. Re-inventory the surviving files and their assertions after the cutover; never recreate tests that S1 replaced at focused live-workspace owners. Other table gates remain unchanged.

Optional early T3a: shared-editor S0b may bring the pure move forward after S0a and before S1 if useful for review. That PR records both IDs and the same pure-move/count evidence; T3a is then delivered and no duplicate later move is scheduled. If S0b is skipped, T3a keeps the default S2b2 gate. S0b changes no fixture or behavior; S1 separately owns all required fixture rewrites and protocol-test dispositions.
