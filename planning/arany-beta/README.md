# Arany beta implementation

## Metadata

- Status: `In progress`
- Owner: Fernando Mirabile
- Last updated: 2026-10-13

## Goal

Deliver the personal Linux beta with durable Sessions/Runs, bounded collaboration,
strict replay, accessible terminal interaction, selected Provider/account access,
opt-in telemetry and guarded files, commands, Skills and local MCP. Broader
platform/release support requires its own native evidence.

Beta 1 also includes multi-task agentic coding teams. That implementation work
has its own self-contained [initiative plan](../agentic-teams/README.md); start
there for any implementation slice. The remaining Beta 1 work outside it is the
user-owned checks and evidence in [next steps](../../NEXT_STEPS.md).

## Remaining execution

[Next steps](../../NEXT_STEPS.md) is the single pending-work list: open defects,
user-deferred Anthropic subscription access, final user
checks and release/platform gates. Select the next authorized implementable
item there; keep unavailable live/native/user checks in the final handoff.

For a selected item, define its successful and meaningful failure/cancellation
paths at the existing contract and evidence owner before implementation. Follow
[the planning workflow](../PLANNING.md), [testing rules](../../agents/testing.md)
and [security rules](../../agents/security.md). User-deferred authorization gates
remain in force; no live call or user credential/clipboard access is implied.

## Contract owners

- [Execution](../../docs/specs/execution.md): Run admission, coordination and recovery.
- [Terminal](../../docs/specs/terminal.md): interaction, output and accessibility.
- [Credentials](../../docs/specs/credentials.md): account scope, storage and recovery.
- [Tools](../../docs/specs/tools.md): grants, effects, Guard and native differences.
- [Diagnostics](../../docs/specs/diagnostics.md): bounded private failure inspection.
- [System overview](../../docs/architecture/system-overview.md): ownership and data flow.

## Exit criteria

Complete the [agentic coding teams plan](../agentic-teams/README.md) and
resolve agreed implementation defects and required verification through their
owners. Report unperformed checks without claiming support; a user-deferred
integration is not completed functionality. Promote accepted behavior and
invariants to specs and code docs and remove completed execution work. Git/CI
preserve completed changes and check results; this plan is not a changelog.
