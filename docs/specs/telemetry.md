# Telemetry

## Scope

Telemetry is optional, lossy output. SQLite Events and replay alone determine Run state, presentation and exit class; export failure never changes them. [Goals](./goals.md) owns Goal and Run segment semantics; [D-14](../research/next-step-decision-register.md#d-14--otlp-is-optional-lossy-output) holds the research. [Next steps](../../NEXT_STEPS.md#beta-2-and-broader-release) owns outstanding verification.

## Endpoint and content

- Telemetry initializes from `--otlp-endpoint` or the supported OTLP variables before the Tokio runtime and Workspace admission. Invalid explicit configuration exits 2 without creating state. An explicit numeric-loopback HTTP endpoint is admitted before Workspace input. The local Collector owns remote routing and authentication. The exporter uses no ambient proxy, redirect, header, resource detector or SDK default destination.
- Only closed typed identifiers, role, phase and outcome enums, bounded model metadata, counts and acknowledged Event sequence and kind cross the trace interface. Prompts, results, summaries, paths, payloads, credentials and raw errors never do.
- Provider categories use the closed `ProviderKind` classification of [native discovery](./provider.md#native-models-and-accounts): native names are `openai`, `anthropic` and `chatgpt`, custom profiles export `custom`, and unrecognized profiles export `other`.

## Spans

- Run spans project committed Events. A Journal subscriber opens and closes each span at its Events' commit times, not from Run-loop clocks, and can neither block nor fail the Run.
- A Goal span describes one admitted Run segment, with its child Run span and only committed Goal lifecycle and reservation markers. It starts at the segment's first committed Goal Event, so no later than its Run span, which runs from `MessageAccepted` to `RunFinished`. It ends at `GoalFinished`, or with the Run when the Goal stays open.
- Agent spans run from `AgentSpawned` to `AgentFinished` and nest by the admitted agent tree. A Provider span runs from its `GoalProviderCallReserved` to its `ProviderCallRecorded`; a call refused before its reservation commits has no span. A Goal Tool-context compaction span runs from `GoalCompactionCallReserved` to `GoalContextCompacted`. Every marker is timestamped at its commit.
- Context compilation precedes every Event, so its measured start and end ride as `arany.context.started_unix_nano` and `arany.context.ended_unix_nano` on the segment's first committed marker.
- A Session compaction outside a Run is its own operation trace; its spans keep the measured call timing and add a marker after `ContextCompacted` commits.
- An exhausted segment exports `failed`. A segment that a cancelled Run leaves open exports the Run's `cancelled` outcome with Goal status `running`; no Goal span reports a cancelled Goal status. Terminal attributes are the closed status, provider-call count, known token usage, an unknown-usage flag and active and uncertain time, plus `arany.run.unguarded_effects` and `arany.goal.unguarded_effects`, the count of attested, uncertain or approved-without-outcome [host commands](./tools.md#primary-host-commands). A later Run or explicit resume starts another segment, because trace context is not persisted. Event markers are capped by the SDK's 32-event span limit and are lossy.
- A Provider span identifies the requested role route, including its closed Provider category and bounded model metadata: the role's route pinned in the committed `RunStarted`, or the agent's latest earlier `ProviderFallbackUsed` switch for that role. A Goal compaction span follows the same switches. A fallback chain shares that logical-step span, and each switch is a `ProviderFallbackUsed` marker on the Run span. Attempts and actual switch identities stay canonical journal facts under the [Provider contract](./provider.md#per-role-selection-and-fallback); the Goal call count charges each attempt.
- A Provider span succeeds only when its response passes Engine semantic and usage validation. Invalid outcomes and usage overflow receive a closed `error.type` class.

A committed `SupervisorVerdictRecorded` emits one `arany.supervisor.verdict` event on its matching Run span at the commit timestamp. It carries only the parallel string arrays `arany.supervisor.signals` (`loop`, `probing`, `disproportion`) and `arany.supervisor.levels` (`notice`, `would_narrow`), in journal signal order, with one to three entries each; features, evidence, candidate responses, subject sequences, sites and payload text are excluded. Without a matching live span nothing is exported. These events share the span event limit with markers: the SDK drops excess events with its dropped-event count, without changing canonical verdicts or slowing the coordinator.

A Provider span of a child aborted by its parent closes as `cancelled`. `abandoned` is reserved for an unknown terminal state.

## Export bounds

- One batch worker retains at most 256 queued spans and exports at most 32 spans per batch, with a 256 KiB encoded body ceiling. A full queue drops spans; a body above the ceiling is refused whole without retry. The Engine never waits on export.
- The [HTTP adapter](../../crates/arany/src/telemetry/http.rs) wraps the complete send and response read in a 500 ms outer deadline; expiry drops that future, releasing an incomplete response's read, before another batch. The locked `opentelemetry-http` Reqwest adapter refuses response bodies above 4 MiB, including streamed ones, independently of the deadline, so a trickling response cannot hold the worker. Shutdown waits at most 750 ms.
- The Journal tap retains 256 committed Events per subscriber. A subscriber that falls further behind receives `Resync` with the missed count, recovers from the view and never slows an append. Agent, Run and Goal spans that lost their closing Event export as `abandoned`, while a Provider or compaction span whose receipt was lost, or whose Run's `RunStarted` was lost, is never created; a span after a lost `ProviderFallbackUsed` names the earlier route.
- Exporter retries are disabled. The local Collector owns buffering and backend retry.
- One export request is not necessarily a whole trace: the batch worker can export before a Run ends.
