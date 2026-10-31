# Telemetry

## Scope

Telemetry is optional, lossy output: SQLite Events and replay alone determine Run state, presentation and exit class, and export failure never changes them. [Goals](./goals.md) owns Goal and Run segment semantics; [next steps](../../NEXT_STEPS.md#beta-2-and-broader-release) owns outstanding verification.

## Endpoint and content

- Telemetry comes from `--otlp-endpoint` or the supported OTLP variables; invalid explicit configuration exits 2 without creating state. An explicit numeric-loopback HTTP endpoint is admitted before Workspace input. The local Collector owns remote routing and authentication; the exporter uses no ambient proxy, redirect, header, resource detector or SDK default destination.
- Only closed typed identifiers, role, phase and outcome enums, bounded model metadata, counts and acknowledged Event sequence and kind are exported; never prompts, results, summaries, paths, payloads, credentials or raw errors.
- Provider categories use the closed `ProviderKind` of [native discovery](./provider/selection.md#native-models-and-accounts): `openai`, `anthropic` and `chatgpt`, `custom` for custom profiles and `other` for unrecognized ones.

## Spans

- Spans project committed Events, timestamped at commit, and can neither block nor fail the Run.
- A Goal span covers one admitted Run segment with its child Run span (`MessageAccepted` to `RunFinished`) and committed Goal lifecycle and reservation markers, ending at `GoalFinished` or with the Run when the Goal stays open. Agent spans run from `AgentSpawned` to `AgentFinished`, nested by the agent tree. A Provider span runs from `GoalProviderCallReserved` to `ProviderCallRecorded`; a Goal Tool-context compaction span from `GoalCompactionCallReserved` to `GoalContextCompacted`. A Session compaction outside a Run is its own trace, with a marker after `ContextCompacted`.
- Context compilation timing rides as `arany.context.started_unix_nano` and `arany.context.ended_unix_nano` on the segment's first committed marker.
- An exhausted segment exports `failed`; a segment a cancelled Run leaves open exports `cancelled` with Goal status `running`. Terminal attributes are the closed status, provider-call count, known token usage, an unknown-usage flag, active and uncertain time, and `arany.run.unguarded_effects` and `arany.goal.unguarded_effects`, counting attested, uncertain or approved-without-outcome [host commands](./tools/host-commands.md#primary-host-commands). A later Run or explicit resume starts another segment. Event markers are capped by the SDK's 32-event span limit and are lossy.
- A Provider span names the requested role route (category and bounded model metadata) from `RunStarted` or the latest earlier `ProviderFallbackUsed` switch; a fallback chain shares one span, each switch a `ProviderFallbackUsed` marker on the Run span. Attempts stay journal facts under the [Provider contract](./provider/selection.md#per-role-selection-and-fallback).
- A Provider span succeeds only when its response passes Engine validation; invalid outcomes and usage overflow get a closed `error.type` class. A child call its parent aborted closes as `cancelled`; `abandoned` is reserved for an unknown terminal state.

A committed `SupervisorVerdictRecorded` emits one `arany.supervisor.verdict` event on its live Run span, carrying only the parallel arrays `arany.supervisor.signals` (`loop`, `probing`, `disproportion`) and `arany.supervisor.levels` (`notice`, `would_narrow`), one to three entries each, never features, evidence or payload text. These share the span event limit; excess events are dropped without changing verdicts.

## Export bounds

- At most 256 queued spans, 32 spans per batch and a 256 KiB encoded body. A full queue drops spans; an oversized body is refused whole. The Engine never waits on export, and one export request may hold part of a trace.
- Each export has a 500 ms deadline and refuses response bodies above 4 MiB. Shutdown waits at most 750 ms.
- The Event tap retains 256 committed Events per subscriber; falling further behind loses spans without slowing a Run. Agent, Run and Goal spans that lost their closing Event export as `abandoned`; Provider and compaction spans with a lost start or receipt are not created.
- Exporter retries are disabled; the local Collector owns buffering and backend retry.
