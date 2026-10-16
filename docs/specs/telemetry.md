# Telemetry

## Scope

Telemetry is optional, lossy output. SQLite Events and replay alone determine Run state, presentation and exit class; export failure never changes them. Goal and Run segment semantics are owned by [Goals](./goals.md), and the research behind the design is in [OTLP observability](../research/otlp-observability.md).

Editing constraints: [security rules](../../agents/security.md).

## Endpoint and content

- Telemetry initializes from `--otlp-endpoint` or the supported OTLP variables before the Tokio runtime and Workspace admission. Invalid explicit configuration exits 2 without state creation. An explicit numeric-loopback HTTP endpoint is admitted before Workspace input. The local Collector owns remote routing and authentication. The exporter uses no ambient proxy, redirect, header, resource detector or SDK default destination.
- Only closed typed identifiers, role, phase and outcome enums, bounded model metadata, counts and acknowledged Event sequence and kind cross the trace interface. Prompts, results, summaries, paths, payloads, credentials and raw errors never do.

## Spans

- A Goal span describes one admitted Run segment, with its child Run span and only committed Goal lifecycle and reservation markers. Its start time is no later than its backdated Run and context-compilation spans.
- A segment that a cancelled Run leaves open exports the Run's `cancelled` outcome with Goal status `running`. No Goal span reports a cancelled Goal status. Terminal attributes are the closed status, provider-call count, known token usage, an unknown-usage flag and active and uncertain time. A later Run or explicit resume starts another segment, because trace context is not persisted. Event markers are capped by the SDK's 32-event span limit and remain lossy.
- A Provider span succeeds only when its response passes Engine semantic and usage validation. Invalid outcomes and usage overflow receive a closed `error.type` class.

A Provider span of a child aborted by its parent closes as `cancelled`. `abandoned` is reserved for an unknown terminal state.

## Export bounds

- One batch worker runs with fixed queue, batch, body, request and shutdown bounds. A full queue drops spans, and the Engine never waits on export.
- Exporter retries are disabled. The local Collector owns buffering and backend retry.
- Product-process OTLP evidence must collect every export request through process shutdown. One request is not necessarily a whole trace, because the batch worker can export before a Run ends.

## Acceptance scenarios and evidence owners

Tests: [`tests/session_run/telemetry.rs::product_exec_exports_only_safe_committed_trace_facts`](../../tests/session_run/telemetry.rs); [`tests/engine_run/goals.rs::exhausted_autonomous_segment_exports_its_failed_status`](../../tests/engine_run/goals.rs); [`tests/engine_run/goals.rs::cancelled_autonomous_segment_exports_its_run_outcome`](../../tests/engine_run/goals.rs).

| Scenario | Required outcome |
|---|---|
| Safe product export | Only closed identifiers, counts and markers cross; prompt and content canaries are absent |
| Goal segment status | An exhausted segment exports `failed`; a cancelled segment of an open Goal exports the Run's `cancelled` outcome with Goal status `running` |
| Export failure | Telemetry failure or a full queue leaves Run state, presentation and exit class unchanged |
