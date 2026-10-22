# Typed judge Provider adapters

## Metadata

- Status: `Draft` (deferred)
- Owner: Fernando Mirabile
- Last updated: 2026-10-22

## Outcome and boundary

Admit a typed judge destination that returns a bounded Boolean, Choice or Score
verdict for [team supervision](../agentic-teams/README.md). This is proposed D
work, separate from already implemented [roster presets](../../docs/specs/provider.md#named-roster-presets).
An ordinary agent-capable roster profile may supply a strict structured review;
a vendor-specific judge capability needs its own researched contract.

## Research gate and slice

No typed-judge vendor API has been verified in this plan. The earlier Jev example
is a candidate, not an adapter selection or compatibility claim. Research one
primary-source API, authentication/consent, verdict semantics, probability
calibration, deadlines, usage and privacy before choosing an adapter.

Then deliver one end-to-end capability through Provider admission and translation,
credential selection, Engine review, journaled verdict, Terminal result and
strict replay. Keep vendor wire types at the Provider edge and reuse existing
routing/account loaders; add no generic plugin or daemon. Each destination is a
separately consented egress path admitted before Workspace disclosure; its verdict
only narrows authority and cannot select another endpoint or credential.
Pin finite request/response, call, token, time, concurrency and retention limits.

Provider owns capability/admission, Credentials owns secret handling and Tools/
Execution own the review boundary. Extend existing Provider wire, routing,
Engine and reducer corpora for valid verdicts, malformed/out-of-range results,
unavailable credentials, timeout, cancellation, exhausted budgets and replay
without another call. Linux/macOS share protocol behavior; live vendor and native
account evidence remains owner-run in [NEXT_STEPS](../../NEXT_STEPS.md).

## Open questions and exit

Owner: Fernando Mirabile. Resolve vendor, schemas, numeric bounds, usage
provenance and calibration before implementation. Close when accepted behavior
moves into specs and the adapter's limits/evidence are explicit; synthetic API
fixtures never establish live judge quality or calibrated probabilities.
