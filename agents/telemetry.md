# Telemetry

Load for `src/telemetry.rs`, its private `telemetry/` files, or Engine/CLI trace instrumentation. The design evidence lives in [OTLP observability](../docs/research/otlp-observability.md); [security.md](./security.md) owns external-I/O trust rules.

- Telemetry is optional lossy output. SQLite Events and replay alone determine Run state, presentation, and exit class. Runtime export failure never changes them.
- Admit an explicit numeric-loopback HTTP endpoint before Workspace input; the local Collector owns remote routing and authentication. The exporter uses no ambient proxy, redirect, header, resource detector, or SDK default destination.
- Pass only closed typed identifiers, role/phase/outcome enums, bounded model metadata, counts, and acknowledged Event sequence/kind. Prompts, results, summaries, paths, payloads, credentials, and raw errors never cross the trace interface.
- A Provider span succeeds only when its response passes Engine semantic and usage validation; `Ok(response)` alone is insufficient. Invalid outcomes and usage overflow get a closed `error.type` class.
- The child coordinator retains each Provider span until its task joins. Close an aborted sibling as `cancelled`; dropping a span as `abandoned` is reserved for an unknown terminal state, not a joined abort.
- Keep one batch worker with fixed queue, batch, body, request, and shutdown bounds. A full queue drops spans; the Engine never waits on export. Disable exporter retries because a retry could outlive the process shutdown bound; the local Collector owns buffering and backend retry. Build before entering the current-thread Tokio runtime and shut down after it returns.
- Product-process OTLP evidence must collect every bounded export POST through process shutdown; one POST is not necessarily a whole trace because the batch worker can export before a Run ends. Gate a split batch when asserting complete topology.
- The binary and library are separate Rust crates. An opaque concrete handle is their configuration bridge, not a substitutable Engine behavior seam; SDK and exporter types stay inside the private telemetry module.
