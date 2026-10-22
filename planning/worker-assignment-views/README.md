# Persistent private worker assignment views

## Metadata

- Status: `Draft`
- Owner: Fernando Mirabile
- Last updated: 2026-10-22

## Outcome and current boundary

**FUTURE proposal:** retain one private Guard view for a worker's entire
assignment, initialized from its assignment-time Workspace snapshot plus its
own acknowledged edits. Preserve a read-only assignment-time build-cache archive;
workers never publish cache updates, and only primary commands publish them.
MCP invocation remains excluded from worker authority.

This restores the remaining C3 proposal, not current behavior or an accepted
change to the [Tools contract](../../docs/specs/tools.md#private-worker-targets-and-handover-transfer).
Today's worker commands reconstruct a fresh private image per command, with no
long-lived native view across model calls. The existing spawn-pinned cache uses
a shared-read lease, writable per-command restoration and discarded updates.
Model-facing commands and the in-view Finish gate already exist; a persistent
assignment lifetime still needs its own admission, ownership and cleanup design.

## Research and direction

[Execution](../../docs/specs/execution.md#worker-journal-protocol) owns assignment
identity, grants, leases, data seals and handover;
[Goals](../../docs/specs/goals.md#worker-accounting-and-verification) owns primary
verification. `tools::worker::View::pin_command_cache` and
`engine/run_loop/worker.rs` pin current assignment-time cache access. Existing
cache machinery alone does not admit a persistent native view.

Retain assignment-local build state without Git worktrees: worktrees introduce
Git metadata without supplying confinement. Commands stay offline and within the
pinned assignment grant; the worker must satisfy its criterion before handover,
and the primary verifies again after integration. Reuse existing Guard and cache
owners where their lifetime guarantees suffice, rather than adding a service.

## Admission gate, slice and evidence

Keep the private command target and shared-read, discard-only cache gated for
this future persistent-view capability until model-facing runtime admission is
implemented for it. This gate does not disable today's per-command runtime.

One vertical slice must carry explicit capability admission through Engine,
journal, Guard lifetime, cache pin, handover and visible settlement. Tools and
Execution must accept the changed lifetime before implementation. Extend their
existing worker/Guard/cache corpora for assignment-local command state, immutable
assignment-time cache reads, no worker publication or MCP invocation, sibling
isolation, criterion refusal, cancellation and crash cleanup. Missing receipts
remain Uncertain and never trigger an automatic retry. Linux and macOS need
equivalent isolation and positively observed disposal; native checks stay in
[NEXT_STEPS](../../NEXT_STEPS.md#pursue-in-order).

## Open questions and exit criteria

Owner: Fernando Mirabile. Resolve native ownership across model calls, crash and
pause disposal, retained storage/task/time limits, cache generation lifetime and
resource admission before implementation. Verify peer behavior from primary
sources under [PLANNING.md](../PLANNING.md), and apply the
[security pass](../../agents/security.md#security-pass-triggers).
This Draft does not authorize persistent storage or change current specs. Close
when the accepted lifetime and its failure/recovery evidence move into the owning
contracts, with unperformed native checks retained in NEXT_STEPS.
