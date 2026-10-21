# Plan title

Keep substantive sections only; delete headings that do not apply. Expand the
boundary-specific questions in [PLANNING.md](./PLANNING.md) only when the change
affects a module boundary. Resolve relative links from the initiative's directory.

## Metadata

- Status: `Draft`
- Owner: `TBD`
- Last updated: `YYYY-MM-DD`

## Outcome and scope

The single feature this plan delivers, the harness responsibility it serves and the work included; exclusions only when they clarify a real boundary. A second feature belongs in its own plan.

## Research

Verified facts with citations: the affected contract, editing rules, implementation and evidence owner, and peer harness behavior read from source. Findings that outlive this plan go to `docs/research/` and are linked here.

## Contract and scenarios

- Owning spec or unmigrated contract, and the proposed change:
- Successful path and meaningful rejection/failure/recovery scenarios:
- Evidence owner and deferred checks:

For work with no observable behavior impact, replace this section with that conclusion.

## Chosen direction

- Simplest sufficient solution after Ponytail:
- Linux/macOS impact and native evidence, or no OS impact:
- Ownership or interface changes, and alternatives for hard-to-reverse decisions:

## Security and resource model

Trust boundaries, attacker-controlled inputs, authorization, secrets, bounds/backpressure and failure containment, or a statement that none change. Triggered work follows the [security-pass requirements](../agents/security.md#security-pass-triggers).

## Risks and mitigations

- Risk → mitigation:

## Vertical slices

1. Slice: the observable behavior it ships, the path it crosses end to end (input → Engine → journal → presentation, as applicable), its spec change, its tests and its PR boundary.
2. Next slice, built on the previous one.

Each slice is usable on its own. Keep only remaining slices as execution progresses.

## Open questions

Material questions that cannot be answered from current sources, with an owner. Delete the section when none remain.

## Exit criteria

Required successful and failure observations with their check owners; unavailable native/live/user checks and their handoff; contract updates or why existing owners remain adequate; closeout under the [planning lifecycle](./README.md#lifecycle).
