# OTLP observability for the Rust-first harness

**Status:** implementation recommendation  
**Researched:** 2026-09-29  
**Scope:** optional OpenTelemetry trace export from the local, CLI-only V1 harness  
**Evidence rule:** protocol and SDK claims below cite primary specifications, official documentation, or upstream source at a reviewed version. Recommendations are labeled separately.

## Executive decision

Add optional OTLP trace export to V1, but only as a second vertical patch after the canonical four-call team run, replay, and cancellation proof passes. It must be disabled unless the operator provides an endpoint. It must not become a condition of Run success.

The smallest appropriate shape is one private `telemetry` module inside the existing Cargo package. That module owns OpenTelemetry SDK construction, a fixed HTTP/protobuf exporter, span mapping, privacy filtering, bounded batching, and shutdown. It exposes typed, synchronous lifecycle handles to the Engine. It does not expose OpenTelemetry types and does not define a trait.

This does not contradict the rule that Provider is the only Engine behavior seam:

- **Provider remains the only substitutable behavior that can change Engine outcomes.** The fake and OpenAI adapters are real alternatives behind that interface.
- **Telemetry is a private output mechanism, not an Engine behavior seam.** Its `Disabled` and `Otlp` modes are concrete implementation states, not injected domain behavior. It receives completed facts and can only observe them.
- An exporter outage, full queue, timeout, partial export, or shutdown failure cannot alter persisted Events, `RunView`, CLI output, cancellation, or exit status.
- No telemetry trait, crate, daemon, global logging façade, or event-consumer interface is earned in V1.

Export **traces only**. Persisted SQLite Events remain the canonical, replayable history. OTLP is a lossy and disposable operational projection. Do not export OpenTelemetry logs in V1 because they would duplicate Events and widen the content-leak surface. Do not export metrics in V1 because periodic metric export is a poor fit for a short-lived CLI and the relevant GenAI metrics conventions are still in development.

Use OTLP over **HTTP/protobuf**, not gRPC or JSON. Pin the OpenTelemetry Rust crates to `0.33.0`, disable default features, and use the blocking reqwest client with the SDK's thread-based batch processor. This avoids adding a second async runtime integration to the current-thread Tokio design. Initialize before entering the Tokio runtime and perform bounded shutdown only after `runtime.block_on(...)` returns.

V1 accepts only a numeric loopback collector endpoint, with redirects and ambient proxies disabled. The local Collector is the security and operations boundary for TLS, authentication, routing, retry, and remote backends. Raw prompts, objectives, instructions, results, summaries, tool arguments/results, file contents/names, provider bodies, headers, and secrets are never telemetry fields.

## Decision table

| Question | V1 decision | Why | Revisit trigger |
| --- | --- | --- | --- |
| Include OTLP now? | Yes, after the core proof, as an optional second patch | It directly serves the requested integration without making observability part of Run correctness | Remove from V1 if it delays the fixed team-run proof |
| Module placement | Private `src/telemetry.rs` inside the existing package | External I/O, privacy, SDK lifecycle, and bounds have enough depth for one module; no crate boundary is earned | Measured dependency/compile, release, privilege, or ownership pressure |
| Engine seam | No new trait or injected behavior seam | Telemetry cannot affect decisions or outcomes; Provider remains the only behavior seam | A second independently owned instrumentation implementation with genuinely different behavior |
| Signal | Traces only | Traces represent nesting, concurrency, latency, and errors; Events already represent durable history | Logs: a concrete diagnostic need not served by safe trace fields. Metrics: daemon or aggregate SLO need |
| Canonical state | SQLite Events and replayed `RunView` only | OTLP queues and collectors are explicitly lossy and external | Never; changing this requires a new durability architecture |
| Transport | OTLP/HTTP with binary protobuf | Smaller feature/runtime surface than gRPC; standard, Collector-compatible path | A required backend lacks HTTP/protobuf, or measurement shows a material deficiency |
| Endpoint | Explicit opt-in, numeric loopback only | Prevents implicit exfiltration and contains SSRF/configuration risk | Direct remote export becomes an explicit product requirement |
| SDK | `opentelemetry*` `0.33.0`, minimal features | Current reviewed Rust release; thread-based batch export fits current-thread Tokio | Normal dependency-review cadence or a security fix |
| Sampling | Always-on | The fixed successful run has only nine spans, so partial traces are not useful | Run topology/volume becomes configurable or large |
| Queue overflow | Drop newly ended spans; never block Engine | Matches SDK behavior and the repository's non-fatal observability rule | A delivery SLO is adopted; prefer Collector durability first |
| Flush | Bounded shutdown after the Tokio runtime returns | Avoids current-thread runtime deadlock and preserves cancellation priorities | Runtime architecture changes |
| Remote TLS/auth | Deferred to local Collector | Keeps credentials and certificate policy outside V1 harness | A user must bypass a local Collector |
| OpenTelemetry logs | Deferred | Duplicates canonical Events and raises privacy risk | Safe noncanonical diagnostics have proven value |
| OpenTelemetry metrics | Deferred | Short process lifetime and developmental GenAI metric semantics | Long-running daemon or aggregate dashboards/SLOs |

## Reconciliation with current repository architecture

The canonical architecture is one package, one process, one current-thread Tokio coordinator, one dedicated SQLite thread, exactly one root plus exactly two concurrent children, and exactly four Provider calls. The terminal contract is append-only human progress or append-only JSONL; it is not an interactive TUI.

The visual review at `/tmp/architecture-review-20260928T213608Z.html` is useful design history, not current authority. It has now been updated to match the canonical architecture. The reconciliation corrected three earlier details that must not reappear in telemetry tests or names:

1. diagrams showing a third worker and text saying “maximum three workers” are superseded by **exactly two children**;
2. “interactive terminal table” is superseded by **append-only human output or JSONL**; and
3. “Provider is the only initial seam” means the only replaceable Engine behavior boundary, not that every external library must live in `lib.rs`.

The private module adds implementation depth while preserving the public shape:

```text
main.rs
  ├── resolves telemetry configuration
  ├── constructs Telemetry before runtime.block_on(...)
  ├── calls Engine with an opaque private observer value
  └── shuts Telemetry down after runtime.block_on(...) returns

lib.rs / Engine
  ├── owns semantic Run, AgentRun, plan, and Provider-call boundaries
  ├── persists Events before publishing committed-event observations
  └── never handles exporters, protocols, headers, or OpenTelemetry types

telemetry.rs (private deep module)
  ├── Disabled | Otlp concrete mode
  ├── safe typed span lifecycle
  ├── semantic-convention and harness-attribute mapping
  ├── privacy allowlist and bounds
  ├── SDK/exporter construction
  └── bounded shutdown and internal diagnostics
```

The Engine already has the semantic facts needed to describe a trace. Passing a private observer by value or reference is not the same as defining a public adapter seam. It is analogous to the current private Store and Workspace mechanisms: complex enough to hide, but not variable domain policy.

## Evidence classification

This report uses three labels:

- **Specification fact** means required or described by an OpenTelemetry specification or semantic-convention document.
- **SDK fact** means behavior of the reviewed OpenTelemetry Rust `0.33.0` implementation and can change between releases.
- **Recommendation** means the harness-specific decision inferred from those facts and repository constraints.

The distinction matters. OTLP wire behavior is stable protocol surface; Rust batch-processor defaults are implementation details; queue size, loopback policy, and attribute selection are this project's choices.

## OTLP protocol and transport

### Stable protocol facts

**Specification fact.** OTLP is encoded with Protocol Buffers and supports gRPC and HTTP transports. The [OTLP specification 1.11.0](https://opentelemetry.io/docs/specs/otlp/) defines default port `4317` for OTLP/gRPC and `4318` for OTLP/HTTP. OTLP/HTTP sends `POST` requests over HTTP/1.1 or HTTP/2.

For traces, metrics, and logs, the default HTTP paths are respectively:

```text
/v1/traces
/v1/metrics
/v1/logs
```

Binary protobuf uses `Content-Type: application/x-protobuf`. JSON is also valid with `Content-Type: application/json`, but it uses protobuf JSON mapping with OTLP-specific details: trace/span IDs are hexadecimal strings, enum values are integers, field names are lower camel case, and 64-bit integers are decimal strings. JSON is therefore an interoperability option, not a simpler internal model.

The protocol specification recommends that receivers accept requests up to 64 MiB and responses up to 4 MiB. Those are interoperability ceilings, not appropriate producer budgets for this harness.

For OTLP/HTTP, `429`, `502`, `503`, and `504` are retryable responses; `400` is not. A successful HTTP status with an OTLP `partial_success` response must be treated as accepted-with-feedback, not retried. Retry behavior uses exponential backoff with jitter and honors server retry guidance within the caller's own deadline. These requirements are defined in the [OTLP protocol](https://opentelemetry.io/docs/specs/otlp/#otlphttp-response) and the [OTLP exporter configuration specification](https://opentelemetry.io/docs/specs/otel/protocol/exporter/).

### Transport decision

**Recommendation.** Fix V1 to HTTP/protobuf:

- It is standard OTLP and supported by the Collector.
- It uses the repository's existing HTTP/TLS ecosystem instead of bringing in tonic and HTTP/2-specific dependencies.
- A blocking client integrates cleanly with the SDK-owned batch thread and does not depend on Tokio executor progress during shutdown.
- Protobuf is the normative compact encoding and avoids JSON mapping edge cases and larger payloads.
- One fixed transport removes configuration states that V1 cannot meaningfully test or support.

Do not compile gRPC, HTTP/JSON, gzip, logs, or metric-provider features merely to make them selectable. Add one only after a concrete receiver requirement or measurement earns it.

## Rust SDK maturity and exact dependency shape

### Maturity snapshot

**SDK fact.** At the reviewed `opentelemetry-0.33.0` tag, released 2026-09-18, the upstream [OpenTelemetry Rust README](https://github.com/open-telemetry/opentelemetry-rust/tree/opentelemetry-0.33.0#project-status) classifies:

- traces API, SDK, and OTLP exporter as **beta**;
- metrics API and SDK as **stable**, with the OTLP exporter at **release candidate**; and
- logs API and SDK as **stable**, with the OTLP exporter at **release candidate**.

The broader OpenTelemetry language page may summarize Rust signals more conservatively. For implementation decisions, the versioned repository's component table is the more precise SDK evidence. Beta does not preclude V1 use, but it means the wrapper must contain SDK types and dependency upgrades must repeat behavior tests.

**SDK fact.** The versioned manifests declare MSRV Rust `1.75` and Apache-2.0 licensing. See the [`opentelemetry` workspace manifest](https://github.com/open-telemetry/opentelemetry-rust/blob/opentelemetry-0.33.0/Cargo.toml) and [`opentelemetry-otlp` manifest](https://github.com/open-telemetry/opentelemetry-rust/blob/opentelemetry-0.33.0/opentelemetry-otlp/Cargo.toml). The project must still run its normal resolved-tree license and advisory review because transitive licenses and advisories are not implied by the top-level crate license.

### Exact candidate dependencies

**Recommendation.** Review and pin this shape in `Cargo.lock`:

```toml
opentelemetry = { version = "0.33.0", default-features = false, features = ["trace"] }
opentelemetry_sdk = { version = "0.33.0", default-features = false, features = ["trace"] }
opentelemetry-otlp = { version = "0.33.0", default-features = false, features = ["trace", "http-proto", "reqwest-blocking-client"] }
```

For tests that inspect completed spans without a network receiver, enable the SDK's `testing` feature only in dev dependencies.

**SDK fact.** `opentelemetry-otlp` enables a broad set by default: traces, metrics, logs, HTTP/protobuf, the blocking reqwest client, and internal logs. `default-features = false` is therefore necessary. The reviewed [feature manifest](https://github.com/open-telemetry/opentelemetry-rust/blob/opentelemetry-0.33.0/opentelemetry-otlp/Cargo.toml) also shows that `http-proto` activates internal trace and metric protocol support even when only a tracer provider is constructed. This is minor unwanted compile surface, not a reason to implement a protocol client; record it in dependency review.

The final `cargo tree -e features` gate must prove that these are absent unless an upstream feature relationship makes one unavoidable:

- tonic/gRPC;
- `rt-tokio` or `rt-tokio-current-thread` SDK integration;
- HTTP/JSON;
- log provider and internal-log bridges;
- metric provider/readers; and
- gzip.

### Current-thread Tokio compatibility

**SDK fact.** The default [BatchSpanProcessor](https://docs.rs/opentelemetry_sdk/0.33.0/opentelemetry_sdk/trace/struct.BatchSpanProcessor.html) owns a dedicated standard-library background thread. The upstream documentation recommends the blocking reqwest client with that thread-based processor, including for Tokio applications. This design does not require OpenTelemetry's Tokio runtime features.

`SdkTracerProvider::shutdown()` and [`shutdown_with_timeout`](https://docs.rs/opentelemetry_sdk/0.33.0/opentelemetry_sdk/trace/struct.SdkTracerProvider.html) are synchronous. The SDK documentation warns against calling blocking shutdown on the main thread of a Tokio current-thread runtime. The processor must join or otherwise coordinate with its worker and exporter; invoking it while the only async executor thread is occupied is a deadlock/starvation risk when an async exporter is involved.

**Recommendation.** Preserve this lifecycle:

```text
parse and validate configuration
construct blocking OTLP exporter and SDK batch thread
construct current-thread Tokio runtime
runtime.block_on(run Engine and complete canonical cleanup)
return to synchronous main
shutdown telemetry with a fixed deadline
render any safe telemetry diagnostic without changing the Run exit code
```

Never call telemetry shutdown from an async Engine future. Do not add `spawn_blocking` or a multi-thread Tokio runtime for telemetry. During cancellation, Engine cancellation, child reaping, terminal Event persistence, and Store close take priority. Only the cleanup time remaining after those operations may be used for telemetry; a second Ctrl-C or exhausted two-second cancellation budget abandons telemetry.

## Signal decision: traces, not logs or metrics

### Traces

Traces answer the operational questions the fixed proof creates: Did both children overlap? Which Provider call was slow? Did the root wait for both? Where did cancellation propagate? They preserve causality and duration without becoming history.

The successful V1 topology is deterministically nine spans:

```text
invoke_workflow harness.team                          (1)
└── invoke_agent harness.root                        (2)
    ├── plan harness.root                            (3)
    │   └── chat <requested-model>                   (4: root planning Provider call)
    ├── invoke_agent harness.worker                  (5)
    │   └── chat <requested-model>                   (6: child A Provider call)
    ├── invoke_agent harness.worker                  (7)
    │   └── chat <requested-model>                   (8: child B Provider call)
    └── chat <requested-model>                       (9: root synthesis Provider call)
```

The child agent spans are explicit children of the root agent span and can overlap. Reverse completion order is valid. Root agent and workflow spans finish only after both child terminal Events and the root terminal Event commit.

### Logs

**Recommendation.** Do not initialize an OpenTelemetry logger provider and do not bridge `tracing` events into OTLP in V1.

SQLite Events already provide ordered, replayable semantic records. Copying them into OTLP logs would create a second history with weaker delivery semantics and invite payload leakage. CLI diagnostics are local operator output, not automatically exportable records. Add OTLP logs only when a concrete production diagnosis cannot be answered by safe trace attributes/events, then define a separate allowlist rather than forwarding all Rust logs.

### Metrics

**Recommendation.** Do not initialize a meter provider in V1. A short-lived CLI would need final collection/export on every invocation, increasing shutdown cost and failure states. Trace attributes can initially carry durations and provider token counts when the provider returns them. The [GenAI metrics conventions](https://github.com/open-telemetry/semantic-conventions-genai/blob/e57c543b4889619eb2a05702471937db5119165d/docs/gen-ai/gen-ai-metrics.md) are also in Development.

Metrics become justified when the harness becomes long-running or operators need aggregate SLOs and dashboards that cannot be computed from collected traces. High-cardinality `run_id` and `agent_run_id` must never become metric labels.

## Canonical Events versus telemetry

**Specification fact.** OpenTelemetry's [error-handling principles](https://opentelemetry.io/docs/specs/otel/error-handling/) require telemetry runtime errors to be handled without taking down the instrumented application. Its [performance guidance](https://opentelemetry.io/docs/specs/otel/performance/) requires bounded resource use and no default blocking on the application path.

**Recommendation.** Enforce a one-way relationship:

```text
semantic transition
  → SQLite transaction commits Event(s)
  → RunView reduces committed Event(s)
  → terminal renders committed Event(s)
  → telemetry may observe a bounded, content-free fact
```

Consequences:

- A span never authorizes, rejects, orders, or rolls back a transition.
- `arany show` replays SQLite only and emits no new run trace.
- Telemetry is not used to recover a Run or rebuild `RunView`.
- A collector acknowledgment is not a durable-delivery guarantee for the harness.
- Telemetry loss cannot change exit code `0`, `1`, `2`, or `130` once Run processing has begun.
- A safe `harness.event.committed` span event may be added only after the transaction commits. It contains kind, sequence, `RunId`, and optional `AgentRunId`, never the Event payload.

Configuration is different from runtime export. Invalid explicit telemetry configuration is an invocation error detected before `RunStarted` and returns exit `2`. A valid configuration whose collector is absent is a non-fatal runtime condition.

## Semantic conventions and custom namespace

### Stability and versioning

**Specification fact.** OpenTelemetry semantic conventions `1.44.0` moved GenAI conventions to the separate [semantic-conventions-genai repository](https://github.com/open-telemetry/semantic-conventions-genai). At reviewed commit [`e57c543b4889619eb2a05702471937db5119165d`](https://github.com/open-telemetry/semantic-conventions-genai/tree/e57c543b4889619eb2a05702471937db5119165d), dated 2026-09-24, agent spans, general GenAI spans, and metrics are all **Development**. The repository has no stable GenAI schema URL to attach at this point.

**Recommendation.** Pin that reviewed commit in the design/test fixture and do not claim stable GenAI-convention conformance. Do not set a GenAI schema URL. Centralize names in `telemetry.rs`; a dependency update does not silently opt into renamed attributes.

### Standard span mapping

Use current standard concepts where they describe the operation:

| Harness operation | Span name | Kind | Important standard attributes |
| --- | --- | --- | --- |
| Full fixed team run | `invoke_workflow harness.team` | `INTERNAL` | `gen_ai.operation.name=invoke_workflow`, `gen_ai.workflow.name=harness.team` |
| Root/child loop invocation | `invoke_agent harness.root` / `invoke_agent harness.worker` | `INTERNAL` | `gen_ai.operation.name=invoke_agent`, low-cardinality `gen_ai.agent.name` |
| Root decomposition | `plan harness.root` | `INTERNAL` | `gen_ai.operation.name=plan`, `gen_ai.agent.name=harness.root` |
| OpenAI Responses request | `chat <requested-model>` | `CLIENT` | `gen_ai.operation.name=chat`, `gen_ai.provider.name=openai`, `gen_ai.request.model`, response model/usage when returned, `server.address`, `openai.api.type=responses` |

These structures follow the Development [agent span conventions](https://github.com/open-telemetry/semantic-conventions-genai/blob/e57c543b4889619eb2a05702471937db5119165d/docs/gen-ai/gen-ai-agent-spans.md), [GenAI span conventions](https://github.com/open-telemetry/semantic-conventions-genai/blob/e57c543b4889619eb2a05702471937db5119165d/docs/gen-ai/gen-ai-spans.md), and [OpenAI-specific conventions](https://github.com/open-telemetry/semantic-conventions-genai/blob/e57c543b4889619eb2a05702471937db5119165d/docs/gen-ai/openai.md). The plan convention explicitly places the inference that produces the plan beneath the plan span. `chat` describes this harness's text-generation use of the Responses endpoint; `openai.api.type=responses` distinguishes the API.

Use `error.type` only with a documented low-cardinality class such as `timeout`, `cancelled`, `http.429`, `invalid_response`, `store`, or `_OTHER`; never include an exception message, URL, model output, or provider response body.

### Harness namespace

The [general semantic-convention naming rules](https://opentelemetry.io/docs/specs/semconv/general/naming/) reserve standard namespaces and recommend a distinct application namespace for nonstandard attributes. Do not invent keys below `otel.*`, `gen_ai.*`, `openai.*`, or another vendor namespace.

Use a documented `harness.*` namespace:

| Attribute/event | Type | Placement | Meaning |
| --- | --- | --- | --- |
| `harness.run.id` | string UUID | workflow and descendant spans | Canonical `RunId` correlation |
| `harness.agent_run.id` | string UUID | agent, plan, and Provider spans | Canonical transient execution attempt |
| `harness.agent_run.parent_id` | string UUID | child agent spans | Canonical parent AgentRun |
| `harness.agent.role` | enum string | agent spans | `root` or `child` |
| `harness.provider.phase` | enum string | Provider spans | `root_plan`, `child_work`, or `root_synthesis` |
| `harness.outcome` | enum string | finished workflow/agent spans | bounded domain outcome |
| `harness.event.committed` | span event | nearest active semantic span | A canonical Event committed |
| `harness.event.sequence` | integer | committed-event span event only | SQLite ordering sequence |
| `harness.event.kind` | enum string | committed-event span event only | bounded Event kind |

Do not put an `AgentRunId` in `gen_ai.agent.id`. The agent convention says that attribute is for a stable provider-hosted agent resource and specifically discourages transient in-memory agent instance identifiers. `harness.agent_run.id` states the actual semantics.

The OpenTelemetry trace ID and span ID remain SDK-generated telemetry identifiers. Do not derive them from UUIDv7 domain IDs and do not persist them as canonical state. Correlation is one-way through the two `harness.*.id` attributes. Do not inject W3C trace context into OpenAI requests: the remote model provider is not a trusted cooperating service. Propagation may be reconsidered for a future owned daemon or Guard boundary.

## Privacy and security contract

### Content denylist

The GenAI conventions mark system instructions and input/output messages as opt-in and warn that they can contain sensitive information. Repository security rules are stricter. The following never enter spans, span events, resources, exporter diagnostics, or test snapshots:

- raw prompts, conversation messages, objectives, agent instructions, summaries, or final results;
- tool names when user/model controlled, arguments, results, stdout/stderr, or payloads;
- included file contents, relative or absolute names, Workspace paths, repository names, or digests;
- provider request/response bodies, structured-output bodies, response IDs, or error bodies;
- API keys, bearer tokens, cookies, headers, certificates, private keys, or environment values;
- usernames, hostnames, command lines, process arguments, or working directories; and
- persisted Event payloads or memory contents.

The module accepts typed metadata structs with only allowed fields. It must not accept `serde_json::Value`, arbitrary key/value iterators, formatted errors, or raw Event/Provider request references. This makes the safe set reviewable at compile time.

Token counts returned as integers, requested/returned model identifiers, fixed role/phase/outcome enums, timings, status codes, and bounded error classes are allowed. Model identifiers need a length limit and control-character rejection; they are operational metadata but can still be user-configured.

### Endpoint and SSRF policy

**Specification fact.** The OTLP exporter configuration standard supports generic and signal-specific endpoints, headers, certificates, client certificates/keys, compression, protocol, and timeout through environment variables. Signal-specific values override generic ones. For HTTP, a generic endpoint has `/v1/traces` appended while the trace-specific endpoint is used as the exact URL. See the [exporter configuration specification](https://opentelemetry.io/docs/specs/otel/protocol/exporter/).

**Recommendation.** V1 deliberately implements a safe subset:

- no endpoint means telemetry is disabled; do not inherit the SDK's implicit `localhost:4318` default;
- endpoint scheme is exactly `http`;
- host is an explicit numeric loopback address (`127.0.0.0/8` or `::1`), not a hostname whose resolution can change;
- reject user information, query, and fragment;
- preserve a validated base path, then append `/v1/traces` for generic/CLI endpoints;
- trace-specific environment endpoint is treated as an exact path;
- redirects are disabled;
- ambient HTTP proxy use is disabled;
- connect and total request timeouts are explicit; and
- no DNS lookup or socket connection occurs during validation.

This policy intentionally allows a local Collector only. It prevents the new flag from becoming an arbitrary URL fetch/exfiltration primitive. Unix-domain sockets could be safer in some deployments but are not standard OTLP endpoint syntax and are not needed for V1.

The upstream [Collector OTLP receiver](https://github.com/open-telemetry/opentelemetry-collector/blob/main/receiver/otlpreceiver/README.md) is stable and defaults to loopback ports `4317` and `4318`. OpenTelemetry's [deployment guidance](https://opentelemetry.io/docs/platforms/linux/configuration/) supports using a local Collector to route telemetry to a backend. The Collector, not the harness, owns remote TLS, authentication, certificate rotation, backend-specific headers, buffering, and routing.

### Ambient configuration and secrets

**SDK fact.** The reviewed HTTP exporter builder merges OTLP header environment variables even when programmatic headers are supplied; see its [HTTP exporter source](https://github.com/open-telemetry/opentelemetry-rust/blob/opentelemetry-0.33.0/opentelemetry-otlp/src/exporter/http/mod.rs). Relying only on `with_headers(empty)` does not prove that ambient secrets cannot be transmitted.

**Recommendation.** Before SDK construction, check only whether unsupported variables are present; do not inspect, display, log, persist, or include their values. V1 rejects the named variables even when their value is empty, covering headers, certificates, client keys/certificates, compression, and unsupported protocols. It also constructs a custom blocking reqwest client with redirects and proxies disabled rather than accepting reqwest ambient behavior.

Construct the Resource programmatically without the environment resource detector. This prevents `OTEL_RESOURCE_ATTRIBUTES` from attaching user/workspace/host metadata. Use only:

```text
service.name = "harness"
service.version = build package version
service.instance.id = random per-process UUID
telemetry.sdk.name / language / version = SDK identity
```

The [Resource SDK specification](https://opentelemetry.io/docs/specs/otel/resource/sdk/) defines resource merging and SDK-provided identity. This minimal explicit Resource avoids host, OS, process command-line, cloud, and environment detectors.

### Future remote export gate

Direct remote endpoints require a separate security design, not merely relaxing the loopback check. It must cover HTTPS-only transport, system/private CA selection, optional mTLS, secret sources for headers without CLI/env disclosure, redirect policy, DNS rebinding and resolved-address validation, private/special-use IP policy, proxy policy, destination allowlists, certificate errors, endpoint ownership, and support diagnostics. Until that gate is complete, users route through the local Collector.

## Deep module alternatives

### Design A — private typed telemetry façade (recommended)

`telemetry.rs` owns every SDK detail and exposes opaque lifecycle types plus fixed metadata records. It can use a concrete internal enum such as `Disabled | Otlp(OtlpTelemetry)` without a trait. Engine callers cannot attach arbitrary content or observe exporter success. If operator feedback about final exporter failures is required, a private generic wrapper may implement the SDK's `SpanExporter` trait, delegate to the concrete OTLP exporter, and retain only a closed error category in an atomic slot. Implementing an upstream SDK trait inside this module does not create an Engine behavior seam.

Benefits:

- smallest API and strongest privacy allowlist;
- OpenTelemetry beta API churn remains in one file;
- parentage is explicit across concurrent tasks;
- no thread-local span guard survives an `.await`;
- no global subscriber/test interference;
- disabled mode is cheap and allocation-free after construction; and
- Provider remains the only domain behavior seam.

Cost: manual mapping of a small, fixed set of Engine operations. That mapping is valuable because it is the privacy boundary.

### Design B — `tracing` macros and a global OpenTelemetry layer

Instrument functions throughout Engine and Provider code, then install a global `tracing` subscriber/layer that exports spans and perhaps logs.

Benefits: familiar Rust ecosystem; convenient diagnostics; possible later log bridge.

Reject for V1 because fields become decentralized, arbitrary formatted values can escape, global state complicates parallel tests and multiple Engine instances, span context across async work becomes less explicit, and it conflates local diagnostics with exported telemetry. Reconsider when a real requirement for structured local logs plus OTLP logs exists and an enforceable field allowlist has been designed.

### Design C — export persisted Events as OTLP logs

Tail the Event journal or mirror each committed Event as a log record.

Reject because Events are durable domain facts while OTLP logs are operational signals. A mirror duplicates history, leaks payloads unless heavily transformed, and cannot accurately represent nested durations or overlapping Provider calls. A committed-event marker inside an existing span gives correlation without making a second event store.

### Design D — custom OTLP protobuf client

Generate the OTLP protobuf types, build requests with the existing HTTP client, and avoid the OpenTelemetry SDK.

Reject because batching, context, span IDs, resource/scope encoding, partial success, retry, export lifecycle, and protocol evolution would become harness code. The reviewed SDK dependency is narrower and more testable than recreating it.

## Proposed module contract

This is an interface sketch, not production code. Names may adjust during test-first implementation, but the information boundary should not expand.

```rust
pub(crate) struct Telemetry { /* Disabled | Otlp, private */ }
pub(crate) struct RunTrace { /* opaque context + workflow span */ }
pub(crate) struct AgentTrace { /* opaque context + agent span */ }
pub(crate) struct PlanTrace { /* opaque context + plan span */ }
pub(crate) struct ProviderTrace { /* opaque context + client span */ }

pub(crate) struct OtlpTraceConfig {
    endpoint: ValidatedLocalTraceEndpoint,
    export_timeout: Duration,
    shutdown_timeout: Duration,
}

impl Telemetry {
    pub(crate) fn disabled() -> Self;
    pub(crate) fn try_otlp(
        config: OtlpTraceConfig,
        build: BuildInfo,
    ) -> Result<Self, TelemetryConfigError>;

    pub(crate) fn begin_run(&self, run_id: RunId) -> RunTrace;
    pub(crate) fn shutdown(self, deadline: Duration) -> TelemetryShutdown;
}

impl RunTrace {
    pub(crate) fn begin_agent(&self, meta: AgentTraceMeta) -> AgentTrace;
    pub(crate) fn event_committed(&self, meta: CommittedEventMeta);
    pub(crate) fn finish(self, outcome: RunOutcome);
}

impl AgentTrace {
    pub(crate) fn begin_plan(&self) -> PlanTrace;
    pub(crate) fn begin_provider(&self, meta: ProviderCallMeta) -> ProviderTrace;
    pub(crate) fn event_committed(&self, meta: CommittedEventMeta);
    pub(crate) fn finish(self, outcome: AgentOutcome);
}

impl PlanTrace {
    pub(crate) fn begin_provider(&self, meta: ProviderCallMeta) -> ProviderTrace;
    pub(crate) fn finish(self, outcome: PlanOutcome);
}

impl ProviderTrace {
    pub(crate) fn finish(self, meta: ProviderOutcomeMeta);
}
```

Contract invariants:

1. All methods after initialization are synchronous, bounded, and infallible from the Engine's perspective.
2. Handles carry explicit parent context and may be moved into child tasks; no entered span guard is held across `.await`.
3. Metadata structs contain typed IDs, enums, numeric counters, and validated bounded model/server identifiers only.
4. `finish` consumes the handle, preventing ordinary double completion. `Drop` may close an incomplete span with a fixed `harness.outcome=abandoned`, but never mutates domain state.
5. `event_committed` is called after the Store acknowledges commit. It accepts no payload.
6. Telemetry initialization may return a configuration error before a Run. Runtime export and shutdown results are diagnostics only.
7. No Engine test requires a collector; deterministic inspection happens behind the private module using the SDK's in-memory exporter under dev features.

The exact composition parameter can remain crate-private. Do not expose this API from `lib.rs` as a public extension point until a second Client or embedder needs it.

## Bounds, batching, failure, and shutdown

### Initial budgets

These are harness recommendations to verify by benchmark, not OpenTelemetry defaults:

| Budget | V1 value | Overflow/deadline behavior |
| --- | ---: | --- |
| Successful fixed run | 9 spans | Test exact topology |
| Batch queue | 256 ended spans | Drop newly ended spans; never block Engine |
| Export batch | 64 spans | Export on batch limit or delay |
| Scheduled delay | 250 ms | Allows a short run to batch without depending on it |
| Attributes per span | 32 | Ignore excess inside mapper; callers cannot supply arbitrary fields |
| Events per span | 32 | Ignore excess and increment internal drop diagnostic at most once |
| Attribute string | 256 UTF-8 bytes | Reject/truncate at validated metadata boundary; never truncate secrets because content is disallowed |
| OTLP request body | 256 KiB | Discard an oversized exporter batch; no unbounded serialization or custom fragmentation |
| Export request timeout | 500 ms | Abandon attempt, keep application running |
| Retry | 1 retry / 2 total attempts | Exponential backoff with jitter: 50 ms initial, 100 ms maximum, 25 ms maximum jitter |
| Normal telemetry shutdown | 750 ms | Stop waiting and report one safe local diagnostic |
| Cancellation total cleanup | Existing 2 s budget | Canonical cleanup first; telemetry gets only remaining time |
| Queued telemetry payload memory | 4 MiB target | Calculated from fixed queue/field limits and verified under saturation; HTTP runtime overhead is measured separately |

**SDK fact.** The Rust batch processor defaults are much larger/slower for this use: queue `2048`, batch `512`, and scheduled delay `5s`. Its bounded queue drops new ended spans when full rather than applying application backpressure. Configure the smaller explicit values through [`BatchSpanProcessorBuilder`](https://docs.rs/opentelemetry_sdk/0.33.0/opentelemetry_sdk/trace/struct.BatchSpanProcessorBuilder.html).

**SDK fact.** `opentelemetry-otlp` `0.33.0` has retry enabled with three retries/four attempts and exponential backoff/jitter defaults; see the versioned [OTLP changelog](https://github.com/open-telemetry/opentelemetry-rust/blob/opentelemetry-0.33.0/opentelemetry-otlp/CHANGELOG.md). That is too expensive for a short CLI. Supply the tighter policy above and ensure a server `Retry-After` cannot exceed the total operation/shutdown deadline.

### Offline collector behavior

An absent/refused/slow Collector is expected operational state:

- span creation and end enqueue only to the bounded SDK queue;
- the exporter thread applies request and retry deadlines;
- no Provider or Store future awaits exporter I/O;
- the private exporter wrapper retains at most one closed failure category for a safe post-run diagnostic; `internal-logs` stays disabled;
- diagnostics mention only that fixed category and the already validated loopback endpoint, never an SDK error string, headers, or bodies;
- Run state and process exit status are unchanged; and
- normal process termination waits at most the shutdown budget.

The SDK cannot provide a durable per-span delivery receipt to the Engine. Successful shutdown means processing completed within the SDK contract, not that a remote backend durably indexed the trace. If future users require delivery guarantees, deploy a local Collector with a persistent queue and define an observability delivery SLO separately; do not put a durable telemetry outbox in the Engine by default.

### Cancellation ordering

On first Ctrl-C:

1. cancel root and both child tasks;
2. abort/reap as defined by the Engine deadline;
3. persist stable cancelled terminal Events best effort;
4. close Store resources;
5. end active spans from known committed outcomes; and
6. after leaving `runtime.block_on`, use only the remaining cleanup budget for telemetry shutdown.

On second Ctrl-C or deadline exhaustion, abandon telemetry immediately. Never call `process::exit` merely to stop the SDK thread; the implementation gate must prove that dropping/timeout behavior allows the process to terminate.

## Configuration contract

### Proposed CLI and environment precedence

One CLI option is sufficient:

```text
arany run --otlp-endpoint http://127.0.0.1:4318 ...
```

Treat the CLI value as a generic base URL and append `/v1/traces`, preserving a validated path prefix according to the OTLP exporter rules.

Resolution order:

1. explicit `--otlp-endpoint`;
2. non-empty `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, treated as the exact trace URL;
3. non-empty `OTEL_EXPORTER_OTLP_ENDPOINT`, treated as a base and appended with `/v1/traces`;
4. absent: telemetry disabled.

The CLI takes precedence over environment, then signal-specific environment takes precedence over generic environment. This is consistent with the standard signal-over-generic relationship while preserving ordinary CLI authority. Empty values count as unset.

Protocol is programmatically fixed to `http/protobuf`. Queue, batch, timeout, retry, privacy, Resource, and sampling values are not V1 configuration. Every option multiplies failure states and weakens the tested contract.

Reject unsupported OTLP variables by name when present, without inspecting or printing values, including generic/trace headers, certificates, client certificate/key, compression, and protocol selectors. This is intentionally narrower than generic OpenTelemetry auto-configuration. The error should identify the unsupported variable name and tell the user to configure the local Collector. Protocol is fixed by the selected builder rather than inferred from environment.

### Opt-in rationale

Explicit opt-in is justified now because:

- sending telemetry is external I/O and must be intentional;
- standard SDK defaults could otherwise contact a local port unexpectedly;
- even content-free identifiers reveal timing and activity;
- an unavailable Collector should not impose startup/shutdown overhead for ordinary users; and
- the core proof must stay independent of an observability stack.

Do not add `--otlp-disabled`, backend names, signal toggles, service-name overrides, arbitrary headers, or sampling flags in V1.

## Deterministic verification

### Unit and golden tests

Use the SDK [`InMemorySpanExporter`](https://docs.rs/opentelemetry_sdk/0.33.0/opentelemetry_sdk/trace/in_memory_exporter/struct.InMemorySpanExporter.html) behind the dev-only `testing` feature. Keep it inside `telemetry.rs` tests so the Engine does not gain an exporter abstraction.

Normalize random trace/span IDs, process instance ID, and timestamps into symbolic values before golden comparison. Assert semantic topology and fields, not map iteration order or SDK-internal debug formatting.

Required deterministic cases:

- successful run yields exactly nine spans, three agent spans, two child agents, one plan, and four Provider spans;
- both child spans overlap and can finish in reverse order;
- root agent/workflow cannot finish before both child terminal commits;
- every parent/child relationship is exact;
- `harness.run.id` and `harness.agent_run.id` correlate correctly without replacing trace/span IDs;
- committed-event sequence numbers appear only after Store acknowledgment and in canonical order;
- Provider timeout, invalid response, cancellation, store failure, and interrupted cleanup map to bounded outcomes/error classes;
- `arany show` emits zero run spans;
- disabled telemetry produces no worker thread/network request and negligible overhead; and
- Drop/incomplete paths close spans without changing Engine state.

### Privacy tests

Seed a canary corpus across objective, `AGENTS.md`, included files, fake Provider input/output, structured response, error body, header, token, filename, absolute Workspace path, summary, and final result. Assert every canary is absent from:

- in-memory `SpanData` names, attributes, events, status descriptions, resources, and instrumentation scope;
- captured encoded protobuf bytes;
- exporter and shutdown diagnostics; and
- golden snapshots.

The test should also prove that arbitrary `OTEL_RESOURCE_ATTRIBUTES` and header variables cannot reach the exporter. Never print those environment values in a failure message.

### Protocol tests

Run a bounded loopback capture server in process, decode the OTLP protobuf request, and assert:

- method `POST`, path `/v1/traces`, and `application/x-protobuf`;
- one explicit Resource and instrumentation scope;
- request/body limit handling;
- HTTP `200` success and OTLP partial-success handling without retry;
- `400` is not retried;
- `429`, `502`, `503`, and `504` retry no more than configured;
- refusal, connection timeout, slow response, malformed response, and oversized response remain non-fatal;
- redirects are not followed and proxy environment variables are ignored;
- headers/cert variables are rejected before any connection; and
- total retry/shutdown time stays within the deadline.

An opt-in acceptance test may run an official Collector pinned by version and image digest with its stable OTLP receiver. Do not use `latest`. A Collector file/debug exporter can prove routing, but it must not become the semantic golden source because exporter stability and formatting can change.

### Runtime and cancellation tests

Create the same current-thread Tokio runtime used in production. Exercise a slow/unavailable receiver while child Provider tasks overlap and cancellation fires. Prove:

- no shutdown is called inside the runtime;
- Provider timers and Ctrl-C cancellation continue to make progress;
- the Store thread and telemetry thread terminate;
- first Ctrl-C honors canonical cleanup and second Ctrl-C abandons telemetry; and
- no test relies on sleep-only synchronization.

Use channels/barriers and a monotonic test deadline. A regression test specifically guards against moving blocking `shutdown` into an async current-thread context.

## Performance budgets

These are provisional gates to measure on a pinned machine/profile, not promises inferred from the SDK:

| Scenario | Budget |
| --- | ---: |
| Disabled instrumentation added to fixed scripted Run | no more than 1% median wall-time regression and 100 µs absolute per Run |
| Enabled, healthy local Collector hot path | no more than 50 µs synchronous cost per span end |
| Enabled, healthy local Collector total Run delta | no more than 5 ms excluding final bounded flush |
| Collector unavailable | no Engine critical-path delay; process termination within 750 ms normal telemetry budget |
| Queued telemetry payload memory | no more than 4 MiB under forced queue saturation; separately report total enabled-process RSS delta |

Use a fast scripted Provider and temporary Store so provider token latency does not hide instrumentation cost. Report distributions, queue drops, allocation counts if available, and build profile. If disabled mode misses its budget, move the conditional to the module boundary; do not make every call dynamically format span data.

## Dependency and maintenance gates

Before merge:

1. pin crate resolution in `Cargo.lock` and record direct crate versions;
2. inspect `cargo tree -e features` for unintended signals, runtimes, transports, compression, and TLS implementations;
3. run the repository's license and vulnerability checks over the resolved graph;
4. verify project MSRV is at least the crates' `1.75` requirement or reject the dependency change;
5. verify only one intended rustls-based HTTP stack/version is resolved where feasible;
6. review the OpenTelemetry Rust changelog and source for queue, retry, shutdown, environment, and feature behavior on every upgrade;
7. rerun privacy/protobuf golden tests when GenAI semantic conventions change; and
8. contain every SDK type inside `telemetry.rs` so removal or upgrade cannot touch domain types.

The new dependency surface is justified only if the trace integration ships in the same patch. Do not add crates as placeholders.

## Implementation sequence and gates

### Gate 0 — preserve the current proof

The fixed team-run architecture test must already pass: one root, exactly two concurrent children, exactly four Provider calls, canonical commit-before-render behavior, replay, and cancellation. OTLP must not be used to debug an incomplete core implementation into existence.

### Gate 1 — configuration and privacy boundary

Implement endpoint parsing, precedence, unsupported-variable detection, typed metadata, and canary tests before constructing an exporter. Invalid explicit configuration exits `2` before `RunStarted`. No secret value is read for diagnostics.

### Gate 2 — in-memory trace model

Build the private module against the in-memory exporter. Prove the exact nine-span tree, reverse child completion, error/cancellation mapping, post-commit event markers, `show` silence, and content exclusion. No network test is needed yet.

### Gate 3 — bounded local OTLP/HTTP exporter

Add the minimal crates/features, custom blocking reqwest client, explicit Resource, batch budgets, retry policy, and loopback protocol tests. A missing Collector must be non-fatal.

### Gate 4 — lifecycle and performance

Prove current-thread Tokio progress and post-runtime bounded shutdown. Run saturation, cancellation, thread cleanup, disabled overhead, healthy collector, and offline collector benchmarks.

### Gate 5 — documentation and dependency review

Document the CLI/environment contract, privacy denylist, Collector example, best-effort delivery semantics, and troubleshooting categories. Review license/advisory/features and update the authoritative architecture/decision register in the implementation change. This research report alone does not change current V1 dependencies.

## Explicit deferrals and triggers

| Deferred capability | Add only when |
| --- | --- |
| OTLP metrics | A daemon/long-running process or aggregate SLO/dashboard cannot be served from traces |
| OTLP logs | A demonstrated diagnosis requires safe noncanonical records and a separate allowlist |
| `tracing`/subscriber bridge | Structured local logging is adopted and global/layer lifecycle plus field privacy are solved |
| Direct remote OTLP | A user must bypass a local Collector and the TLS/auth/secret/SSRF design passes review |
| OTLP/gRPC | A required backend lacks HTTP/protobuf or benchmarks show a meaningful need |
| OTLP/HTTP JSON | A required receiver supports only JSON |
| Compression | Measured payload/network cost outweighs dependency/CPU/config cost |
| Configurable sampling | Trace volume grows beyond the fixed tiny topology |
| Persistent telemetry queue | A written delivery SLO exists; prefer Collector persistent queue first |
| Context propagation | A trusted owned downstream process/Guard exists and its trust boundary is explicit |
| Separate observability crate | Measured dependency compile cost, independent release, ownership, or privilege boundary appears |
| Public telemetry extension API | A second real Client/embedder needs to control observation behavior |

## Final recommendation

OTLP is feasible and fits the Rust-first harness without weakening the core architecture if it is treated as optional operational output rather than state or policy. Implement a trace-only, HTTP/protobuf, local-Collector integration in one private module after the minimum team proof. Keep Provider as the only Engine behavior seam, SQLite Events as the only canonical history, and the terminal as append-only human/JSONL output.

The implementation is ready to plan when these non-negotiable gates are accepted:

- exact two-child/four-Provider-call trace model;
- disabled by default and local numeric loopback only;
- no prompts/results/tool content/secrets in any signal;
- fixed minimal Resource and no ambient resource/header configuration;
- minimal `0.33.0` features with blocking HTTP exporter and SDK batch thread;
- explicit bounded queue, request, retry, memory, and shutdown behavior;
- shutdown after leaving current-thread Tokio;
- telemetry failures never alter a started Run's truth or exit status; and
- in-memory, protobuf, cancellation, privacy, and performance evidence before merge.

No additional research is required to start that implementation. Remote export, logs, metrics, propagation, and broader configuration each require their own trigger and security gate rather than speculative V1 surface.

## Primary sources

- [OpenTelemetry Protocol specification 1.11.0](https://opentelemetry.io/docs/specs/otlp/)
- [OTLP exporter configuration specification](https://opentelemetry.io/docs/specs/otel/protocol/exporter/)
- [OpenTelemetry error handling](https://opentelemetry.io/docs/specs/otel/error-handling/)
- [OpenTelemetry performance guidance](https://opentelemetry.io/docs/specs/otel/performance/)
- [OpenTelemetry Resource SDK specification](https://opentelemetry.io/docs/specs/otel/resource/sdk/)
- [Semantic convention naming](https://opentelemetry.io/docs/specs/semconv/general/naming/)
- [OpenTelemetry Rust `0.33.0`](https://github.com/open-telemetry/opentelemetry-rust/tree/opentelemetry-0.33.0)
- [`opentelemetry-otlp` `0.33.0` feature manifest](https://github.com/open-telemetry/opentelemetry-rust/blob/opentelemetry-0.33.0/opentelemetry-otlp/Cargo.toml)
- [`opentelemetry-otlp` `0.33.0` changelog](https://github.com/open-telemetry/opentelemetry-rust/blob/opentelemetry-0.33.0/opentelemetry-otlp/CHANGELOG.md)
- [Rust SDK `BatchSpanProcessor`](https://docs.rs/opentelemetry_sdk/0.33.0/opentelemetry_sdk/trace/struct.BatchSpanProcessor.html)
- [Rust SDK `SdkTracerProvider`](https://docs.rs/opentelemetry_sdk/0.33.0/opentelemetry_sdk/trace/struct.SdkTracerProvider.html)
- [GenAI agent conventions at reviewed commit](https://github.com/open-telemetry/semantic-conventions-genai/blob/e57c543b4889619eb2a05702471937db5119165d/docs/gen-ai/gen-ai-agent-spans.md)
- [GenAI span conventions at reviewed commit](https://github.com/open-telemetry/semantic-conventions-genai/blob/e57c543b4889619eb2a05702471937db5119165d/docs/gen-ai/gen-ai-spans.md)
- [OpenAI GenAI conventions at reviewed commit](https://github.com/open-telemetry/semantic-conventions-genai/blob/e57c543b4889619eb2a05702471937db5119165d/docs/gen-ai/openai.md)
- [GenAI metric conventions at reviewed commit](https://github.com/open-telemetry/semantic-conventions-genai/blob/e57c543b4889619eb2a05702471937db5119165d/docs/gen-ai/gen-ai-metrics.md)
- [Collector OTLP receiver](https://github.com/open-telemetry/opentelemetry-collector/blob/main/receiver/otlpreceiver/README.md)
- [Collector deployment guidance](https://opentelemetry.io/docs/platforms/linux/configuration/)
