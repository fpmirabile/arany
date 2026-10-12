# Resolve the native macOS stopped-command timeout attribution

## Metadata

- Status: `In progress`
- Owner: Arany maintainers
- Last updated: `2026-10-12`
- Source baseline: `5b3c21e`
- Next action: decide whether the bounded investigation can close with the defect open, or define a new attribution campaign.

## Remaining decision

The approved initial campaign is exhausted. It reproduced case timeouts with
useful stage capture, but did not demonstrate a unique cause. The agreed exits
covered a demonstrated repair and diagnostic delivery without reproduction.
Closing after an attributed case recurrence without a demonstrated cause needs
an explicit decision; it must not silently become a claim that the defect is
fixed. Implementation and PR publication remain authorized.

Keep the unresolved defect and optimized/account evidence in
[Next steps](../../NEXT_STEPS.md). The [fixture rules](../../agents/guard-fixture.md)
own capture activation, privacy, bounds and cancellation observations; the
[Guard rules](../../agents/guard.md) own fresh catalogue checks and exact literal
permissions. [Primary-source research](../../docs/research/macos-system-catalogue-profile-rules.md)
explains the profile encoding. Completed checks belong in the PR and handoff.

## If attribution continues

Before executing another native campaign, define a concrete contrast between
profile compilation, checked-file revalidation, storage preparation and host
scheduling. Measure the responsible boundary directly; Ready is an aggregate
observation. Fix the narrow demonstrated owner and protect the failure through
the existing [native Engine journey](../../tests/session_run/tools_macos.rs).
A passing run alone does not establish the original cause.

Preserve the 30-second case and 180-second parent deadlines, the five stopped
outcomes, closed replay and native cleanup ownership. Do not retry until green,
cache authority metadata, widen permissions or turn private fixture capture
into product tracing. The [Tool](../../docs/specs/tools.md) and
[execution](../../docs/specs/execution.md) contracts remain the behavior owners;
follow the [spec workflow](../../docs/specs/README.md) for any changed requirement.

## Closeout

If the bounded investigation is accepted as complete, leave the defect open in
Next steps and remove this execution plan under the
[planning lifecycle](../README.md#lifecycle). Otherwise retain only this remaining
work and resolve the campaign decision before further reproduction attempts.
