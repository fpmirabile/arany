# Telemetry

## Scope

Telemetry is optional, lossy output. SQLite Events and replay alone determine Run state, presentation and exit class; export failure never changes them. Goal and Run segment semantics are owned by [Goals](./goals.md), and the research behind the design is in [OTLP observability](../research/otlp-observability.md).

Editing constraints: [security rules](../../agents/security.md).

## Endpoint and content

- Telemetry initializes from `--otlp-endpoint` or the supported OTLP variables before the Tokio runtime and Workspace admission. Invalid explicit configuration exits 2 without state creation. An explicit numeric-loopback HTTP endpoint is admitted before Workspace input. The local Collector owns remote routing and authentication. The exporter uses no ambient proxy, redirect, header, resource detector or SDK default destination.
- Only closed typed identifiers, role, phase and outcome enums, bounded model metadata, counts and acknowledged Event sequence and kind cross the trace interface. Prompts, results, summaries, paths, payloads, credentials and raw errors never do.

## Spans

- Run spans are a projection of committed Events. A Journal subscriber opens and closes each span at its Events' commit times, not from clocks inside the Run loop, and it can neither block nor fail the Run.
- A Goal span describes one admitted Run segment, with its child Run span and only committed Goal lifecycle and reservation markers. It starts at the segment's first committed Goal Event, so no later than its Run span, which runs from `MessageAccepted` to `RunFinished`. It ends at `GoalFinished`, or with the Run when the Goal stays open.
- Agent spans run from `AgentSpawned` to `AgentFinished`, and a Provider span from its `GoalProviderCallReserved` to its `ProviderCallRecorded`; a call refused before its reservation commits has no span. A Goal Tool-context compaction span runs from `GoalCompactionCallReserved` to `GoalContextCompacted`. Every marker is timestamped at its commit.
- Context compilation precedes every Event, so its start and end are the two measured timings no Event carries. They ride as `arany.context.started_unix_nano` and `arany.context.ended_unix_nano` on the segment's first committed marker.
- A Session compaction outside a Run is its own operation trace; its spans keep the measured call timing and add a marker after `ContextCompacted` commits.
- A segment that a cancelled Run leaves open exports the Run's `cancelled` outcome with Goal status `running`. No Goal span reports a cancelled Goal status. Terminal attributes are the closed status, provider-call count, known token usage, an unknown-usage flag and active and uncertain time, plus `arany.run.unguarded_effects` and `arany.goal.unguarded_effects`, the count of attested, uncertain or approved-without-outcome [unguarded re-runs](./tools.md#unguarded-re-run-after-a-guard-failure). A later Run or explicit resume starts another segment, because trace context is not persisted. Event markers are capped by the SDK's 32-event span limit and remain lossy.
- A Provider span identifies the requested role route, including its closed Provider category and bounded model metadata: the role's route pinned in the committed `RunStarted`, or the agent's latest earlier `ProviderFallbackUsed` switch for that role. A configured fallback chain shares that logical-step span, and each switch is a `ProviderFallbackUsed` marker on the Run span; individual attempts and actual switch identities are canonical `ProviderFallbackUsed` facts under the [Provider contract](./provider.md#per-role-selection-and-fallback), and the Goal call count charges each attempt.
- A Provider span succeeds only when its response passes Engine semantic and usage validation. Invalid outcomes and usage overflow receive a closed `error.type` class.

A Provider span of a child aborted by its parent closes as `cancelled`. `abandoned` is reserved for an unknown terminal state.

## Export bounds

- One batch worker runs with fixed queue, batch, body, request and shutdown bounds. A full queue drops spans, and the Engine never waits on export.
- The Journal tap retains 256 committed Events per subscriber. A subscriber that falls further behind receives `Resync` with the missed count and never slows an append; agent, Run and Goal spans that lost their closing Event export as `abandoned`, while a Provider or compaction span whose receipt was lost, or whose Run's `RunStarted` was lost, is never created; a span after a lost `ProviderFallbackUsed` names the earlier route.
- Exporter retries are disabled. The local Collector owns buffering and backend retry.
- Product-process OTLP evidence must collect every export request through process shutdown. One request is not necessarily a whole trace, because the batch worker can export before a Run ends.

## Acceptance scenarios and evidence owners

| Scenario | Required outcome | Owner |
|---|---|---|
| Per-role request attribution | A child span uses its selected Provider category and model while preserving the admitted agent tree; fallback attempts retain canonical journal provenance. | `crates/arany/src/telemetry/tests.rs::direct_and_team_span_trees_follow_admitted_agents` |
| Fallback attribution | A switched call keeps its requested route, the switch is a Run marker, and the agent's next call and a Goal compaction follow the committed switches | `crates/arany/src/telemetry/tests.rs::fallback_switches_mark_the_run_and_keep_each_call_on_its_requested_route` |
| Safe product export | Only closed identifiers, counts and markers cross; prompt and content canaries are absent | [`crates/arany-cli/tests/session_run/telemetry.rs::product_exec_exports_only_safe_committed_trace_facts`](../../crates/arany-cli/tests/session_run/telemetry.rs), [telemetry boundary](../../crates/arany/src/telemetry.rs) |
| Goal segment status | An exhausted segment exports `failed`; a cancelled segment of an open Goal exports the Run's `cancelled` outcome with Goal status `running` | [Telemetry journey](../../crates/arany-cli/tests/session_run/telemetry.rs) |
| Export failure | Telemetry failure or a full queue leaves Run state, presentation and exit class unchanged | [telemetry boundary](../../crates/arany/src/telemetry.rs) |
| Commit-time spans | Spans start and end at their Events' commit times; context compilation timing rides on the first marker; a segment without its Run receipt exports `abandoned` | [`crates/arany/src/telemetry/tests.rs::spans_start_and_end_at_their_events_commit_times`](../../crates/arany/src/telemetry/tests.rs) |
| Lagging subscriber | Appends never wait for a subscriber; a lagging one receives `Resync` and recovers from the view | [`crates/arany/src/journal.rs::lagging_subscriber_resyncs_from_the_view_without_blocking_appends`](../../crates/arany/src/journal.rs) |
