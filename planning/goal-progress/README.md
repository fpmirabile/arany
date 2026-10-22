# Goal activity presentation

## Metadata

- Status: `Draft`
- Owner: Fernando Mirabile
- Last updated: 2026-10-22

## Outcome and proposal

Present the current Goal's work, agents/profiles, usage, pending questions and
Supervisor notices together. [Terminal](../../docs/specs/terminal.md) owns the
current Run/agent views; [Goals](../../docs/specs/goals.md) owns durable facts.
The following extends those views and is not implemented behavior.

Compose activity lines in Engine from committed Events without model calls.
Each agent outcome may carry a progress note of at most 160 characters; replace
its live line as notes arrive and retain history in agent inspection. Show tokens
and calls per profile without cost estimates. At [RPI](../goal-rpi/README.md)
phase transitions the primary may add one paragraph of narration. This avoids
model-generated checklists while keeping receipts as the source of truth.
Linear and screen-reader views carry the same labeled facts.

## Slice and verification

One slice carries bounded progress facts through Provider outcome admission,
Engine/journal/replay, Goal view, inspection and JSONL. Update Goals/Execution,
Provider and Terminal only for the accepted additions, using existing reducer,
Session and terminal journeys. Verify streaming replacement, restart history,
oversized notes, cancelled agents, narrow layout and equivalent accessible
output. Reuse existing modules; no separate progress service. Linux/macOS share
presentation semantics; real emulator/screen-reader evidence stays in
[NEXT_STEPS](../../NEXT_STEPS.md#terminal-continuity-evidence).

## Open questions and exit

Owner: Fernando Mirabile. Decide note encoding, byte/retention/cadence limits,
Event ownership and narration budgeting; character count alone does not bound
serialized storage. Research peer Goal views before implementation. Notes are
untrusted display/context data and grant nothing; apply the
[rendering security rules](../../agents/security.md).
Close when accepted activity/usage/question facts agree with replay across all
views and remaining native checks have named owners.
