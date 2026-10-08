# Plan title

## Metadata

- Status: `Draft`
- Owner: `TBD`
- Last updated: `YYYY-MM-DD`

## Goal

Describe the observable outcome in one short paragraph.

Name the harness responsibility this serves, or state that the change is development workflow only.

## Scope

- In scope:
- Out of scope:

## Current state

- Record verified facts about the repository, behavior, and environment.
- Link the relevant research, ADRs, module rules, and interfaces.

## Product contract and acceptance scenarios

- Owning spec or unmigrated contract:
- Agreed behavior and proposed change:
- Successful path and meaningful rejection/failure/recovery scenarios:
- Existing evidence owner and deferred checks:
- Contract review: required updates or why the existing contract covers the work:

For work with no observable behavior impact, replace this section with that conclusion. Use the [spec workflow](../docs/specs/README.md) for contract review and changes.

## Ownership and seams

| Responsibility or state | Owning module | Interface | Seam category | Adapters |
|---|---|---|---|---|
| | | | | |

Describe invariants, dependency direction, and the information each module hides.

## Constraints

- Technical:
- Compatibility:
- Performance and resources:
- Security and privacy:
- Operational:

For implementation work, apply [Arany's priorities](../AGENTS.md#product-and-implementation-priorities): record the simplest sufficient approach after Ponytail and the Linux/macOS/Windows impact, including necessary differences and native verification. State when there is no OS impact.

## Options considered

Keep this section only when the decision was genuinely close.

### Option A

- Summary:
- Depth and locality:
- Benefits:
- Costs:

### Option B

- Summary:
- Depth and locality:
- Benefits:
- Costs:

## Chosen direction

- Decision:
- Reason:
- Rejected alternatives:

Resolve uncertainty that changes this decision before implementation or list it under `Open questions`.

## Interface and protocol impact

- Commands and results:
- Events and ordering:
- Cancellation and shutdown:
- Error taxonomy:
- Versioning and capability negotiation:
- Replay and idempotency:
- Cross-language fixtures or SDK impact:

Delete this section when the plan has no interface or protocol impact.

## Security and resource model

- Trust boundaries and attacker-controlled inputs:
- Authorization and capabilities:
- Secrets and sensitive data:
- Bounds and backpressure:
- Failure containment and cleanup:

## Risks and mitigations

- Risk:
  Mitigation:

## Execution

Use one ordered list for a cohesive PR. Split into tickets only when one PR would mix responsibilities or exceed a safe review boundary.

1. First coherent step and its verification.
2. Second coherent step and its verification.
3. Final integration, documentation, and cleanup.

### Ticket template

- Status:
- Title:
- Goal:
- Scope:
- Depends on:
- Verification:
- PR boundary:

## Open questions

- Include only material questions that cannot be answered from current sources.
- Assign an owner or decision mechanism when known.
- Delete this section when no open questions remain.

## Exit criteria

- Observable behavior:
- Correctness and recovery:
- Performance and resources:
- Security:
- Documentation and auto-learning:
- Closeout: update the owning spec, route evidence and pending checks to their existing owners, update inbound links and delete the completed plan:

## Change log

- `YYYY-MM-DD`: Created plan.
