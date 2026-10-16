# OTLP observability: design trade-offs

**Research baseline:** 2026-09-29, including the OpenTelemetry Rust `0.33.0`
source review. [Cargo.lock](../../Cargo.lock) and the [dependency inventory](../security/beta-dependency-inventory.md)
own the current resolved build; the baseline is not a release-support claim.

[Telemetry spec](../specs/telemetry.md) owns endpoint admission, privacy,
resource bounds, SDK integration and shutdown. [Architecture](../architecture/system-overview.md)
owns placement. This report retains the reasons behind those choices.

## Traces answer a different question from history

SQLite Events describe durable Session and Run facts. Traces describe duration,
causality and overlap. Export queues and Collectors are lossy, so a trace cannot
determine replay, completion or an exit class. Dynamic parentage follows actual
admitted work rather than a fixed illustrative span count.

Trace-only export avoids mirroring sensitive history as logs or adding metric
readers for a short-lived process without an aggregate service requirement.
Local development diagnostics now have their own [contract](../specs/diagnostics.md);
they do not justify exporting their content. Logs, metrics and direct remote
telemetry each need a demonstrated use case and separate privacy design.

## A typed private boundary

| Approach | Assessment |
|---|---|
| Concrete private telemetry owner | Central field allowlist and explicit lifecycle; manual mapping is the privacy control |
| Global tracing subscriber and scattered fields | Convenient instrumentation, but decentralized formatting and global state complicate privacy and parallel tests |
| Mirror persisted Events as OTLP logs | Duplicates history, exposes payload risk and poorly represents concurrent duration |
| Implement OTLP directly | Makes batching, identifiers, resource encoding, partial success and protocol evolution harness responsibilities |

The reviewed SDK supplies the protocol machinery. SDK types stay private;
callers cannot attach arbitrary attributes or make decisions from export success.
Explicit parentage avoids keeping a thread-local span guard across an await.
A failed or cancelled operation must retain its real terminal classification,
not become successful merely because a transport future returned.

## HTTP/protobuf and a local Collector

OTLP/HTTP with binary protobuf uses the existing HTTP ecosystem and avoids a
second gRPC stack or protobuf-JSON mapping. A fixed transport narrows the states
the harness must admit and verify. Protocol interoperability ceilings are not
appropriate producer memory budgets.

An explicit numeric-loopback Collector keeps backend authentication, TLS and
routing outside Arany's exporter. Ambient destination, proxy or resource-detector
defaults could disclose data without the intended opt-in. The local Collector
owns backend buffering and retry; the harness remains bounded and disposable.
HTTP status alone is not complete OTLP delivery evidence: partial-success
responses have protocol meaning, and a process can export several batches.

## Executor and shutdown ownership

The reviewed default batch processor owns a standard-library thread. Its worker
executor is not the main Tokio reactor, so an async HTTP adapter must own a
reactor on which its complete request and response can make progress. A timeout
around only connection setup does not bound a trickling response.

SDK shutdown is synchronous. Calling it on the current-thread coordinator can
starve the async work needed to finish. Construct and close the telemetry owner
at the process lifetime boundary; Run cancellation and durable cleanup take
priority. Queue overflow drops telemetry rather than blocking the Engine.
[Telemetry spec](../specs/telemetry.md) owns the exact implementation and
limits; no duplicate SDK-default or feature table is maintained here.

## Semantics and evidence

Developmental GenAI conventions can change and may describe content-bearing
fields. Use only reviewed names and closed metadata compatible with the privacy
boundary, rather than adopting an entire convention or automatic detector.
Dependency upgrades must review feature expansion and worker behavior.

A synthetic Collector and SDK inspection can prove topology, allowlisted fields,
multiple batches, saturation and bounded failure. They cannot establish remote
delivery or a backend's retention policy. [Testing rules](../../agents/testing.md)
own evidence requirements; [next steps](../../NEXT_STEPS.md) owns missing checks.

## Primary sources

- [OTLP specification](https://opentelemetry.io/docs/specs/otlp/), [exporter configuration](https://opentelemetry.io/docs/specs/otel/protocol/exporter/), [error handling](https://opentelemetry.io/docs/specs/otel/error-handling/)
- [OpenTelemetry Rust at the reviewed tag](https://github.com/open-telemetry/opentelemetry-rust/tree/opentelemetry-0.33.0), [OTLP feature manifest](https://github.com/open-telemetry/opentelemetry-rust/blob/opentelemetry-0.33.0/opentelemetry-otlp/Cargo.toml)
- [BatchSpanProcessor](https://docs.rs/opentelemetry_sdk/0.33.0/opentelemetry_sdk/trace/struct.BatchSpanProcessor.html), [SdkTracerProvider](https://docs.rs/opentelemetry_sdk/0.33.0/opentelemetry_sdk/trace/struct.SdkTracerProvider.html)
- [GenAI agent conventions at the reviewed commit](https://github.com/open-telemetry/semantic-conventions-genai/blob/e57c543b4889619eb2a05702471937db5119165d/docs/gen-ai/gen-ai-agent-spans.md)
- [Collector OTLP receiver](https://github.com/open-telemetry/opentelemetry-collector/blob/main/receiver/otlpreceiver/README.md)
