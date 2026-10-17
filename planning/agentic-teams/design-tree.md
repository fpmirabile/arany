# Agentic coding teams: design rationale

Rationale for the remaining [team plan](./README.md#chosen-direction). The plan
owns proposed requirements; implemented behavior belongs to the linked specs.
Completed slices and the grilling transcript remain in Git history.

## Worker ownership

| Choice | Reason | Remaining slice |
|---|---|---|
| AgentRun identity is the assignment attempt | A new AgentRun for reassignment makes stale work distinguishable without a separate attempt counter or fencing token. Existing researcher identity does not yet supply worker lease/base pins. | C2 |
| Journaled handover eligibility | Late, duplicate, superseded or cancelled results must not publish; eligibility follows recorded facts rather than arrival timing. | C2 |
| Fixed, non-overlapping Run-scoped leases | The existing Goal writer lock excludes outside writers; leases handle internal ownership without renewal clocks or cross-Run fencing. | C2 |
| Completion-order integration and waves | Disjoint leases permit faster feedback; recording integration order makes replay deterministic without a general task DAG. | C2 |
| Digest conflicts require primary resolution or reassignment | Automatic text merge could silently lose a worker's edits. | C2 |
| Disposable worker storage and no automatic restart | Recovery stays at a Run boundary; recovering a half-built view would need another durability contract. | C2/C3 |
| Worker-local loss differs from uncertain Workspace publication | Only primary integration can leave the Workspace uncertain; private-view loss can discard one worker without failing the whole Run. | C2 |
| Whole bounded changesets | Rejecting an oversized changeset avoids partial publication. | C2 |
| Edit-only workers before command-capable views | Writable teams can arrive before the Supervisor; the primary verifies criteria after C2 integration until C3 can verify inside each view. | C2/C3 |
| Persistent private Guard views, without Git worktrees | Workers need assignment-local build state and isolation; worktrees bring `.git` without confinement. | C3 |
| Criterion before handover and verification after integration | In-view checks catch worker breakage; primary checks catch interaction effects. | C3 |
| Read-only assignment-time build cache | Publishing worker build state could expose outputs from work later discarded. Repeated local builds are the accepted cost. | C3 |
| MCP catalog listing only | MCP effects can escape the worker view and lease, so those boundaries cannot contain an invocation. | C2/C3 |

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
