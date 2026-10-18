# Agentic coding teams: design rationale

Rationale for the remaining [team plan](./README.md#chosen-direction). The plan
owns proposed requirements; implemented behavior belongs to the linked specs.
Completed slices and the grilling transcript remain in Git history.

## Worker ownership

Implemented C2 decisions are owned by [Execution](../../docs/specs/execution.md#worker-journal-protocol), [Session identity and replay](../../docs/specs/session.md#worker-replay) and [Goal recovery](../../docs/specs/goals.md#worker-accounting-and-verification). The remaining choices below constrain C3.

| Choice | Reason | Remaining slice |
|---|---|---|
| Disposable worker storage and no automatic restart | Recovery stays at a Run boundary; recovering a half-built view would need another durability contract. | C3 |
| Edit-only workers before command-capable views | Writable teams can arrive before the Supervisor; the primary verifies criteria after C2 integration until C3 can verify inside each view. | C3 |
| Persistent private Guard views, without Git worktrees | Workers need assignment-local build state and isolation; worktrees bring `.git` without confinement. | C3 |
| Criterion before handover and verification after integration | In-view checks catch worker breakage; primary checks catch interaction effects. | C3 |
| Read-only assignment-time build cache | Publishing worker build state could expose outputs from work later discarded. Repeated local builds are the accepted cost. | C3 |
| MCP catalog listing only | MCP effects can escape the worker view and lease, so those boundaries cannot contain an invocation. | C3 |

## Supervisor and routing

| Choice | Reason | Remaining slice |
|---|---|---|
| Engine-owned, configurable evaluators | Deterministic signals and interchangeable model judges should consume journaled evidence without becoming authority sources. A judge API needs primary-source research before an adapter. | B |
| Shadow mode for fuzzy signals; exact success gates enforce | Loop, probing and disproportion thresholds need calibration; receipt matching is deterministic. The Goal success gate already lives in the [Goals contract](../../docs/specs/goals.md#initial-plan-contract-and-success-receipt). | B2/B5 |
| Review points plus opt-in per-effect judging | Trajectory review belongs before integration and Finish; judging every effect needs an explicit cost choice. | B4/C4 |
| Private roster and preset admission | Eligibility, credentials and disclosure must be fixed by the user before project input; model selection can only narrow admitted roles. | D |
| Pause roster-bound agents after exhausted routing | Independent work can continue until it needs the failed result. Existing [per-role fallback](../../docs/specs/provider.md#per-role-selection-and-fallback) supersedes the earlier broad fallback proposal; it does not yet implement roster-agent pause. | D |

## Goal phases and user control

| Choice | Reason | Remaining slice |
|---|---|---|
| RPI phases as separate Runs, with one mandatory Plan gate | Approval waits hold no Run deadline, and recovery can stop at a durable phase boundary. | B6 |
| Private Session artifacts and bounded grilling | Research and plan data need durable inspection without writing proposal files into the user's repository. | B6/E2 |
| Headless RPI requires an explicit plan-approval flag | Scriptable `exec` retains its upfront grant; recommended grilling answers become visible assumptions. | E10 |
| Engine activity plus bounded progress notes, without checklists | Journaled activity supplies progress without model calls; notes explain intent without replacing receipts. | E7 |
| Private pre-images and reverse changesets for rewind | Explicit digest-checked restoration preserves drift detection and avoids Git ghost commits. Command and MCP effects stay explicitly non-rewindable. | E4 |
| Per-folder user authorization before project rules can request commits | Repository text can guide conventions but cannot grant itself host Git authority. Remote and disruptive writes retain explicit human gates. | E6 |
| One-use Supervisor overrides | A recorded user decision can calibrate a signal without silently changing configuration. | E5 |
| Proxy-only registry fetch, followed by offline builds | Dependency retrieval needs a narrow public-registry grant without letting project scripts use the network. | A6 |

## Decisions awaiting implementation data or owner resolution

Follow the [plan's open-question routing](./README.md#open-questions):
[roadmap item 6](../../NEXT_STEPS.md#pursue-in-order) owns RPI exhaustion and Auto
authority, item 12 owns Supervisor threshold calibration, and the
[E4 freeze entry](../../NEXT_STEPS.md#freeze-until-item-11-lands) owns checkpoint quotas.
Native build and team measurements remain roadmap evidence, not design proof.
Multiple supervisors and cross-Session memory require a demonstrated need
before expanding this scope.
