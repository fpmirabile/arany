# Plan title

Keep substantive sections only. Expand the boundary-specific questions in
[PLANNING.md](./PLANNING.md) when the change needs them; a small task does not
need a table for every possible interface or risk.
Resolve relative links from the initiative's directory when copying this template.

## Metadata

- Status: `Draft`
- Owner: `TBD`
- Last updated: `YYYY-MM-DD`

## Outcome and scope

Describe the result, the harness responsibility it serves and the work included.
Name exclusions only when they clarify an actual scope boundary. For development
workflow changes, state that scope directly.

## Current facts and owners

Link the affected contract, editing rules, implementation and evidence owner.
Record verified facts needed for the decision; do not copy their full contents.

## Product contract and acceptance scenarios

- Owning spec or unmigrated contract:
- Agreed behavior and proposed change:
- Successful path and meaningful rejection/failure/recovery scenarios:
- Existing evidence owner and deferred checks:
- Contract review: required updates or why the existing contract covers the work:

For work with no observable behavior impact, replace this section with that conclusion. Use the [spec workflow](../docs/specs/README.md) for contract review and changes.

## Chosen direction

- Simplest sufficient solution after Ponytail and why it fits:
- Linux/macOS/Windows impact and native evidence, or no OS impact:
- Ownership or interface changes, if any:
- Alternatives for hard-to-reverse decisions and meaningful trade-offs:

For an affected boundary, describe state ownership, adapters, ordering, errors,
cancellation, compatibility and replay. Resolve material uncertainty or name it
under `Open questions`; a proposal is not an accepted contract.

## Security and resource model

- Trust boundaries and attacker-controlled inputs:
- Authorization and capabilities:
- Secrets and sensitive data:
- Bounds and backpressure:
- Failure containment and cleanup:

State when no security boundary or resource behavior changes. For triggered
work, use the [security-pass requirements](../agents/security.md#security-pass-triggers).

## Risks and mitigations

- Risk:
  Mitigation:

## Execution

Use one ordered list for a cohesive PR. Split into tickets only when one PR would mix responsibilities or exceed a safe review boundary.

1. First coherent step and its verification.
2. Second coherent step and its verification.
3. Final integration, documentation, and cleanup.

For multiple slices, name each slice's owner, dependencies, verification and PR
boundary. Keep only remaining work and the next action as execution progresses.

## Open questions

- Include only material questions that cannot be answered from current sources.
- Assign an owner or decision mechanism when known.
- Delete this section when no open questions remain.

## Exit criteria

- Required successful and meaningful failure observations, with check owners:
- Unavailable native/live/user checks and their handoff:
- Contract and instruction updates, or why existing owners remain adequate:
- Closeout under [planning lifecycle](./README.md#lifecycle): route durable knowledge and pending checks, update inbound links and remove completed execution work.
