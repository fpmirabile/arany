# Per-agent recovery after exhausted routes

## Metadata

- Status: `Draft` (deferred)
- Owner: Fernando Mirabile
- Last updated: 2026-10-22

## Outcome and research boundary

Pause an agent after its admitted routes are exhausted while independent work
continues; let the user retry or switch within admitted authority. This is the
remaining D recovery proposal. [Provider](../../docs/specs/provider.md#named-roster-presets)
already owns named presets and assignment selection (`e53fafba` #102,
`c6fe23ec` #110, `81e856b2` #115). Their current ordered HTTP-5xx fallback,
fail-and-drain behavior and explicit new-Run recovery remain authoritative.
This plan does not change roster/account admission.

## Direction and slice

Extend the existing Engine/reducer state rather than adding another scheduler.
One vertical slice carries exhausted-route pause, dependency blocking,
user retry/switch, active-time/budget accounting and explicit restart recovery
through Execution/Goals Events, Provider route admission and Terminal/Serve.
Only eligible pre-admitted routes may be selected; unknown effect outcomes stop
without retry. Credentials stay with existing loaders and never reach workers.
Use the existing routing, worker, reducer and product journeys for independent
sibling continuation, dependent blocking, cancelled/failed profiles, restart,
budget exhaustion and refusal of unadmitted destinations.

## Open questions and exit

Owner: Fernando Mirabile. Define accepted pause/retry/switch Events, dependency
semantics, finite waits/queued-work limits and user commands before implementation;
verify peer exhausted-route behavior from primary sources. Linux/macOS share
policy; native account/terminal support retains the checks in
[NEXT_STEPS](../../NEXT_STEPS.md#pursue-in-order). Exit when the accepted lifecycle
is replayable without repeating uncertain work and belongs in its owning specs.
