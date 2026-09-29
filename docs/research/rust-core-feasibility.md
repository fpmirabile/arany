# Rust as the core of an AI agent harness

**Research date and source access date:** 2026-09-28  
**Question:** Is Rust a feasible choice for the frontend-independent core of an agent harness whose CLI, TUI, web, desktop, and future clients are separate adapters—and can it make harness-owned latency negligible beside model latency?

> **Minimum-scope update — 2026-09-29:** This report proves the broader Rust feasibility and preserves later expansion evidence. Its early `SQLite WAL + FTS5`, daemon/protocol, Artifact, streaming, multi-crate, and `tracing`-facade recommendations are not the first physical slice. The current implementation choices are [the minimum architecture](../architecture/system-overview.md), [the next-step decision register](./next-step-decision-register.md), and [the OTLP report](./otlp-observability.md): one process, rollback `DELETE + EXTRA`, no FTS, no daemon, no Artifacts, bounded non-streaming OpenAI, one Provider behavior seam, and optional trace-only OTLP in a private module after the core proof.

## Executive verdict

**Yes: Rust is a strong choice, with one major qualification.** It is particularly well matched to a long-lived orchestration core that owns run state machines, incremental streaming, bounded concurrency, subprocess supervision, local persistence, policy enforcement, and a stable frontend protocol. Rust gives this layer memory safety without a garbage collector, deterministic destruction, native process APIs, and a single-binary deployment path on the main desktop/server platforms ([Rust ownership](https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html), [Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html)). OpenAI Codex and AAIF Goose are substantial, current Rust agent harnesses with multiple clients; they are strong feasibility precedents, although they are **not** comparative benchmark evidence ([OpenAI Codex](https://github.com/openai/codex), [Goose](https://github.com/aaif-goose/goose), [Goose custom-client architecture](https://github.com/aaif-goose/goose/blob/main/CUSTOM_DISTROS.md)).

The exact goal that “the only slowness is token generation” is not physically achievable. End-to-end latency also includes UI and IPC, admission queues, state reads and durable writes, context construction, retrieval and embedding calls, request serialization, DNS/TLS/network transit, provider queueing and prompt prefill, tool I/O, and frontend rendering. OpenAI itself separates network, prompt-processing, and generation latency and says generation is usually—not exclusively—the largest component ([OpenAI latency guide](https://developers.openai.com/api/docs/guides/latency-optimization), [production latency lifecycle](https://developers.openai.com/api/docs/guides/production-best-practices)). The defensible objective is:

> Every stage owned by the harness has an explicit latency and resource budget, stays bounded under overload, and adds only a small measured fraction of representative end-to-end latency.

The principal Rust risk is **provider integration velocity**, not runtime feasibility. As of the access date, OpenAI lists Rust only under community libraries while Go is an official beta SDK; Google’s official GenAI SDK list omits Rust and explicitly recommends direct REST for languages such as Rust; Anthropic also lacks an official Rust SDK. AWS is the notable counterexample: its official Rust SDK includes Bedrock Runtime and streaming examples ([OpenAI libraries](https://developers.openai.com/api/docs/libraries), [Google GenAI libraries](https://ai.google.dev/gemini-api/docs/libraries), [Google partner integration guidance](https://ai.google.dev/gemini-api/docs/partner-integration), [Anthropic SDK overview](https://platform.claude.com/docs/en/cli-sdks-libraries/overview), [AWS SDK for Rust](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/welcome.html), [Bedrock streaming example](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/rust_bedrock-runtime_code_examples.html)). Day-zero support for new provider features and schema changes must therefore be treated as a first-class product cost.

**Recommended shape:** build one Rust engine crate, use it in-process for Rust-native clients when useful, and expose the stable public seam through a local daemon/process using a small versioned command/event protocol. Keep model providers, persistence, retrieval, MCP, tool execution, sandboxing, and telemetry behind narrow internal interfaces. Start with SQLite WAL + FTS5 and an immutable application event journal. Add embeddings and a vector index only after retrieval evaluations justify them. Keep local inference behind the same provider interface; do not embed an inference runtime into the MVP.

## Decision summary

| Decision | Recommendation | Why |
|---|---|---|
| Core language | **Rust** | Strong fit for bounded async orchestration, native process control, predictable resource ownership, portable binaries, and the stated learning goal. |
| Public frontend seam | **Versioned process protocol** | Language-neutral, crash-isolated, reconnectable, and not tied to Rust ABI. |
| Internal hot path | **Typed in-process channels** | Avoids serialization when the frontend is Rust and embedded; [Codex uses this shape](https://github.com/openai/codex/blob/main/codex-rs/app-server-client/README.md). |
| Client protocol | **Spike ACP vs a small custom contract** | ACP offers ecosystem interoperability but is broader/evolving; a custom event contract can be smaller and precisely replayable. |
| Tool/extension protocol | **MCP** | Current official Rust SDK support is strong; MCP is not the frontend protocol. |
| Canonical memory | **Immutable event journal** | Auditable, replayable, independent of provider retention and derived indexes. |
| MVP database | **SQLite WAL + FTS5** | Embedded deployment, transactions, concurrent readers, adequate local lexical search. |
| Vector search | **Defer; keep a `RetrievalIndex` port** | Vectors are derived data and add quality, consistency, migration, and operational costs. |
| Large outputs | **Content-addressed artifact/blob store** | Prevents large tool results from bloating hot rows, queues, and context. |
| Native plugins | **Do not load Rust dynamic libraries** | Rust ABI has no stability guarantee; a bad plugin shares the core’s failure domain. |
| Extensions | **MCP processes first; WASI components later** | Process isolation now; capability-oriented typed sandbox when the extension API stabilizes. |
| Local models | **Ollama/llama.cpp/vLLM adapter** | Preserves provider independence; decoding speed is an inference-runtime/GPU concern, not an orchestration-language concern. |

## What Rust can—and cannot—make faster

### Latency decomposition

For one turn, use this model rather than a single undifferentiated “response time”:

```text
user action
  → UI serialization / local IPC
  → admission and scheduling
  → state load / event append
  → working-context construction
  → optional retrieval / embedding / reranking
  → provider request encoding
  → DNS + connect + TLS + network
  → provider queue + prompt prefill + reasoning
  → token decoding
  → incremental provider stream parsing
  → event persistence / fan-out / UI render
  → optional tool approval + process/network I/O
```

Rust can reduce and stabilize the stages in the harness’s ownership: scheduling overhead, accidental copying, queueing, stream relay, context assembly, process supervision, and local storage coordination. It cannot remove provider queueing, network distance, durable `fsync`, embedding API latency, prompt prefill, GPU/CPU model decoding, or a slow external tool. Streaming reduces perceived time to first output but does not remove generation work ([OpenAI streaming guide](https://developers.openai.com/api/docs/guides/streaming-responses), [Ollama streaming](https://docs.ollama.com/api/streaming)).

Local inference does not change that separation. Ollama exposes local and cloud models over HTTP and streams newline-delimited JSON; llama.cpp and vLLM expose OpenAI-compatible HTTP surfaces, with documented compatibility limitations ([Ollama API](https://docs.ollama.com/api/introduction), [llama.cpp server](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md), [vLLM OpenAI-compatible server](https://docs.vllm.ai/en/latest/serving/openai_compatible_server/)). Put them behind `Provider`. Rust may make the orchestration around a local model efficient; token throughput, prompt prefill, batching, quantization, and GPU utilization belong to the chosen inference runtime. A native Rust inference engine can be evaluated later without changing the engine interface.

### Proposed latency budgets, not measured results

These are initial acceptance targets for a defined reference machine, release build, warm process, local frontend, and no retrieval unless stated. They must be tuned after measurement; they are **not claims about current performance**.

| Owned stage | Proposed warm target | Measurement boundary |
|---|---:|---|
| In-process command admission | p99 ≤ 0.5 ms | caller send → engine acceptance |
| Local process protocol admission | p99 ≤ 2 ms | client write complete → engine acceptance |
| No-retrieval turn preparation | p95 ≤ 25 ms | acceptance → first provider request byte attempted |
| Warm lexical retrieval + context construction | p95 ≤ 75 ms at target corpus | retrieval start → provider request ready |
| Provider-byte relay | p99 ≤ 2 ms | provider byte/event received → local client event visible |
| Normal snapshot + tail recovery | p95 ≤ 200 ms | session open → reconstructed run ready |
| Cancellation propagation | p99 ≤ 50 ms to owned async tasks; separately measure child termination | cancel accepted → cancellation observed |
| Representative end-to-end workload | owned harness stages ≤ 5% of p95 wall time | trace-derived sum, excluding provider/network/tool wait |

Durable append latency must be reported separately for each SQLite synchronous/checkpoint policy and storage device. It is misleading to bury an `fsync` inside “core overhead.” Likewise, cold start, cold cache, model load, schema migration, and first TLS connection must have separate distributions.

## Architecture: deep modules and deliberate seams

In this report, a **module** is any unit with an **interface** and hidden **implementation**; an interface includes types, invariants, errors, configuration, and performance behavior. A **seam** is the exact boundary where replacement is intended, and an **adapter** is one concrete implementation of that interface. **Depth** is the leverage obtained when a small interface hides substantial policy and mechanics. **Locality** means a likely change—such as an OpenAI event rename—stays concentrated in one place. Use a deletion test for proposed seams: if removing an adapter would not simplify the rest of the system, the seam is probably misplaced or premature.

### Recommended system shape

```text
 CLI       TUI       desktop       web       automation
  │         │           │           │             │
  ├── typed in-process adapter ──────┤             │
  └──── versioned stdio / socket / HTTP+WS adapter ┴────┐
                                                       │
                         Command / Event protocol       │
                                                       ▼
┌─────────────────────────────────────────────────────────────┐
│ Rust engine                                                 │
│                                                             │
│  Engine / run actors / policy / context compiler            │
│       │          │            │              │              │
│       ▼          ▼            ▼              ▼              │
│  ProviderPort  ToolPort  SessionStore  RetrievalIndex       │
│       │          │            │              │              │
│ cloud/local   process/MCP  SQLite/Postgres  FTS/vector      │
│                                                             │
│  immutable events → projections/snapshots → bounded fan-out │
└─────────────────────────────────────────────────────────────┘
```

### The public engine interface

Keep the stable interface small. An indicative shape is:

```rust
Engine::open(Config, Dependencies) -> EngineHandle
EngineHandle::submit(Command) -> CommandId
EngineHandle::events(Cursor) -> Stream<EventEnvelope>
EngineHandle::shutdown(Deadline) -> ShutdownReport
```

`EventEnvelope` should carry at least a stable session/run identifier, a monotonic per-run sequence number, event kind and schema version, causation/correlation identifiers, and either a typed payload or artifact reference. This interface hides scheduling, providers, storage, snapshots, retrieval, fan-out, and cancellation. The same semantic contract is implemented by two real adapters: an in-process Rust adapter and a serialized process adapter. That passes the “two implementations” test without inventing abstraction for its own sake.

The event subscription contract must define replay, cursor expiry, slow-consumer behavior, and terminal-event guarantees. A slow client gets a bounded buffer; if it falls behind, text deltas may be coalesced, or the client is disconnected with the last delivered sequence and reconnects from that cursor. Lifecycle events, errors, approvals, tool transitions, and final results must never be silently dropped. Durable sequence IDs plus replay make frontend correctness independent of fan-out speed.

### Suggested modules

| Module | Small interface hides | Adapters / implementations | Locality benefit |
|---|---|---|---|
| `domain` | Run/turn/tool-call state transitions and invariants | Pure reducers/state machines | Agent-loop changes do not touch HTTP, DB, or UI. |
| `engine` | Use cases, actor ownership, cancellation tree, admission | Tokio runtime implementation | Runtime policy remains out of frontends. |
| `protocol` | Versioned commands, events, cursors, capability negotiation | In-process, stdio/socket, HTTP+SSE/WS | Frontends share one semantic contract. |
| `providers` | Request, event stream, cancellation, capability discovery, raw metadata | OpenAI, Anthropic, Gemini, Bedrock, Ollama, llama.cpp, vLLM | Provider churn stays inside one adapter crate. |
| `memory` | Append/replay/snapshot; context compilation; retention | SQLite; later Postgres | Storage details do not infect the run state machine. |
| `retrieval` | Query, filters, result provenance and score semantics | FTS5; later pgvector/LanceDB/Qdrant | Vector choice remains reversible. |
| `artifacts` | Bounded put/get by content identity | Filesystem; object store later | Large outputs stay out of queues and event rows. |
| `tools` | Capability check, approval, execution, normalized result | Built-ins, supervised process, MCP | Tool trust and transport stay explicit. |
| `sandbox` | Requested limits → effective capability report | Linux, macOS, Windows, external runner | Platform security change is localized. |
| `telemetry` | Internal spans/events/metrics and redaction policy | `tracing`, OTLP exporter | Domain code does not depend on exporter types. |

Avoid a universal repository trait or a giant “backend” interface. Give the event journal, artifacts, retrieval index, provider, and tool runtime separate semantics and failure modes. Keep these ports internal until a second real implementation or test adapter makes the seam valuable.

An indicative workspace—not a mandate to make every module a crate—is:

```text
crates/
  arany-domain/            # pure commands, events, reducers, invariants
  arany-engine/            # use cases, run actors, cancellation, context compiler
  arany-protocol/          # versioned wire DTOs and compatibility fixtures
  harness-providers/       # provider port plus initially small adapters
  harness-memory/          # journal, snapshots, projections, retrieval port
  harness-tools/           # policy, process supervisor, MCP integration
  harness-telemetry/       # tracing facade and redaction
bins/
  aranyd/                  # process/daemon adapter
  arany-cli/               # thin client
```

Keep `arany-domain` free of Tokio, HTTP, SQL, MCP, and frontend types. Do not split every provider or storage adapter into a crate on day one; split when independent dependencies, release cadence, compile cost, or ownership creates a real locality benefit. Crates are compile-time boundaries, while modules inside a crate can still be deep.

### Library, daemon, or both?

| Shape | Advantages | Costs |
|---|---|---|
| Rust library only | Lowest in-process overhead; typed calls; simplest embedded CLI/TUI | Rust-only public consumer surface; UI crash shares process; hard upgrades/reconnect; ABI cannot support arbitrary native clients. |
| Daemon only | Language-neutral; crash isolation; one owner of state/tools; reconnect/replay; independent frontend releases | Serialization and IPC; lifecycle/auth/versioning; another process to package and supervise. |
| **Engine crate + daemon adapter** | Fast internal path plus stable external seam; shared semantics; testable transports | Must prevent behavior drift between adapters; slightly more design work. |

Choose the third. Treat the Rust crate API as an internal build-time interface and the versioned process protocol as the compatibility promise. OpenAI Codex provides a useful precedent: its app-server client uses typed in-process channels on the hot path, serializes JSON only at stdio/WebSocket boundaries, bounds runtime queues, and performs bounded graceful shutdown; its external app-server protocol uses JSON-RPC-style JSONL/WebSocket messages ([Codex app-server client](https://github.com/openai/codex/blob/main/codex-rs/app-server-client/README.md), [Codex in-process server](https://github.com/openai/codex/blob/main/codex-rs/app-server/src/in_process.rs), [Codex core](https://github.com/openai/codex/blob/main/codex-rs/core/README.md)). One Codex-local subscriber queue is currently unbounded, so the precedent should inform the shape, not be copied without review.

Goose provides a second precedent: a Rust core is used by CLI, desktop, and API surfaces, and its current direction is a single ACP-based client protocol exposed in-process, through stdio, and over HTTP/WebSocket ([Goose custom distributions](https://github.com/aaif-goose/goose/blob/main/CUSTOM_DISTROS.md), [Goose client/server roadmap](https://github.com/aaif-goose/goose/discussions/7697)). Again, this proves practical feasibility, not performance superiority.

### ACP versus a custom client protocol; MCP is separate

ACP standardizes communication between editors/clients and coding agents and has official Rust and TypeScript libraries. Its Rust SDK supports core roles plus HTTP/SSE and WebSocket transports, but its v2 surface is explicitly draft and unstable as of the access date ([ACP organization](https://github.com/agentclientprotocol), [ACP Rust SDK](https://github.com/agentclientprotocol/rust-sdk), [ACP TypeScript SDK](https://github.com/agentclientprotocol/typescript-sdk)).

Run a short design spike:

- Map required run lifecycle, event replay, approvals, reconnect, artifacts, multi-client observation, and capability negotiation onto stable ACP.
- Measure how many custom methods and semantic workarounds are required.
- Compare it with a small, versioned `Command`/`EventEnvelope` contract.
- Prefer ACP if interoperability is valuable and the extension surface stays modest; prefer the custom contract if durable replay/multi-client semantics become mostly proprietary extensions.

Do **not** confuse this decision with MCP. ACP or the custom contract is the client/frontend seam. MCP is an extension/tool/resource seam.

## Async streaming and systems-memory discipline

### Boundedness is more important than micro-allocation wins

Tokio’s bounded MPSC channels apply backpressure when capacity is reached, whereas unbounded channels can buffer arbitrarily and exhaust process memory ([Tokio channels tutorial](https://tokio.rs/tokio/tutorial/channels), [Tokio MPSC](https://docs.rs/tokio/latest/src/tokio/sync/mpsc/mod.rs.html), [unbounded-channel warning](https://docs.rs/tokio/latest/tokio/sync/mpsc/fn.unbounded_channel.html)). Tokio’s scheduler fairness guarantee also assumes that task count is bounded and tasks do not block the runtime thread indefinitely ([Tokio runtime behavior](https://docs.rs/tokio/latest/tokio/runtime/)).

Therefore every queue, cache, stream, and concurrency class needs:

- A capacity in bytes or work units, not only item count.
- An overflow action: wait, reject, coalesce, spill to an artifact, or disconnect.
- Queue-time and depth telemetry.
- A cancellation and shutdown rule.
- A statement of what “accepted” means: durably committed versus admitted into volatile memory.

Bound provider concurrency globally and per provider, tool concurrency globally and per trust class, database reads, subscriber fan-out, request bodies, SSE/WS frames, stdout/stderr, prompt tokens, artifact sizes, and pending approvals. Prefer one mutable owner/actor per active run over an `Arc<Mutex<World>>` graph.

### Streaming

Reqwest exposes response body streams and a reusable client with connection pooling; Axum can return SSE and accept WebSockets ([Reqwest response](https://docs.rs/reqwest/latest/reqwest/struct.Response.html), [Reqwest client](https://docs.rs/reqwest/latest/reqwest/struct.Client.html), [Axum SSE](https://docs.rs/axum/latest/axum/response/index.html), [Axum WebSockets](https://docs.rs/axum/latest/axum/extract/ws/index.html)). The provider reader should incrementally parse typed provider events, normalize them, and publish without waiting for a complete model response. It should not synchronously persist each token with a separate durable transaction. Coalesce adjacent text deltas under a short byte/time threshold while persisting semantic boundaries and the final logical message under an explicit durability policy.

Provider adapters must tolerate evolution. Anthropic’s SSE stream uses named lifecycle/content events and its versioning policy allows new event variants, so an unknown event must be preserved or safely ignored rather than crashing the run ([Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming), [Anthropic versioning](https://platform.claude.com/docs/en/api/versioning)). Gemini offers both SSE content streaming and a stateful bidirectional Live WebSocket API with interruption/resumption semantics, which should be represented as adapter capabilities rather than leaking into the core state machine ([Gemini streaming API](https://ai.google.dev/api/generate-content), [Gemini Live API](https://ai.google.dev/api/live)).

### Cancellation and task ownership

Tokio cancellation is cooperative. Dropping the losing future in `select!` cancels that future, but cleanup and cancellation-safety depend on the operation; hierarchical `CancellationToken`s provide explicit signalling ([Tokio `select!`](https://tokio.rs/tokio/tutorial/select), [CancellationToken](https://docs.rs/tokio-util/latest/tokio_util/sync/index.html)). `spawn_blocking` work cannot be aborted once running and can delay shutdown indefinitely, so it is not an acceptable home for untrusted or potentially stuck tool execution ([Tokio `spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)).

Give each run a root cancellation token and children for provider, retrieval, summarization, tool, persistence, and subscriber work. Supervise every task: no detached task may own a child process, database transaction, stream, semaphore permit, or canonical state change. Cancellation behavior must be defined per boundary:

- Provider HTTP: stop reading and drop/close the request; call a provider cancel endpoint when available.
- Database: roll back unfinished transactions; never acknowledge an event before its promised durability point.
- Derived jobs: make summary/embedding/index work idempotent and safe to retry.
- Child tools: close stdin, allow a bounded grace period, terminate the entire process group/job, then reap.
- Frontends: emit or replay one terminal cancellation outcome even when completion races cancellation.

### Ownership and “zero-copy” realism

Rust ownership gives memory safety without garbage collection and drops values when ownership ends; the checks do not require a runtime garbage collector ([Rust ownership](https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html)). That does not make Rust allocation-free or immune to leaks, deadlocks, logical races, retained buffers, or `unsafe`/FFI errors ([unsafe Rust](https://doc.rust-lang.org/book/ch19-01-unsafe-rust.html), [behavior not considered unsafe](https://doc.rust-lang.org/reference/behavior-not-considered-unsafe.html)). `Arc` uses atomic reference counting and cycles can leak ([Rust `Arc`](https://doc.rust-lang.org/std/sync/struct.Arc.html)).

`bytes::Bytes` supports cheap clones and slices over shared backing storage, and Serde can borrow data when the input buffer outlives the decoded value ([`Bytes`](https://docs.rs/bytes/latest/bytes/struct.Bytes.html), [Serde lifetimes](https://serde.rs/lifetimes.html)). But streaming I/O generally cannot lend transient buffers across arbitrary task/storage lifetimes, escaped JSON strings may require allocation, IPC serializes data, TLS/kernel boundaries still copy or transform, and a small shared slice can retain a large backing buffer. Use borrowing, `Bytes`, preallocation, and `serde_json::RawValue` where a profile shows value; do not promise end-to-end zero copy ([`RawValue`](https://docs.rs/serde_json/latest/serde_json/value/struct.RawValue.html)).

## Memory architecture: canonical history, working context, and retrieval

“Agent memory” should not be one database column or one ever-growing prompt. Separate data by authority and lifecycle:

1. **Canonical execution history:** immutable user, model, tool, approval, cancellation, and error events.
2. **Working state:** current run status, pending tool calls, capabilities, policies, and active plan, reconstructed by a reducer.
3. **Working context:** the bounded, provider-specific request assembled for the next inference.
4. **Long-term memory:** curated facts, preferences, decisions, episodes, and procedures with provenance and scope.
5. **Artifacts:** large tool output and files stored by content identity, referenced from events.
6. **Derived data:** snapshots, summaries, embeddings, FTS/vector indexes, and caches; all rebuildable.

Research supports explicit memory tiers and selective retrieval rather than blindly placing all history in every prompt. MemGPT proposes virtual context tiers; Generative Agents combines stored observations, reflection, and dynamic retrieval; Reflexion uses a bounded episodic buffer; RAPTOR retrieves across recursively summarized abstraction levels; and “Lost in the Middle” demonstrates that long context availability does not guarantee reliable use of all positions ([MemGPT](https://arxiv.org/abs/2310.08560), [Generative Agents](https://doi.org/10.1145/3586183.3606763), [Reflexion](https://papers.neurips.cc/paper_files/paper/2023/hash/1b44b878bb782e6954cd888628510e90-Abstract-Conference.html), [RAPTOR](https://openreview.net/pdf?id=GN921JHCRw), [Lost in the Middle](https://direct.mit.edu/tacl/article/doi/10.1162/tacl_a_00638/119630/Lost-in-the-Middle-How-Language-Models-Use-Long)). These papers motivate experiments; they do not prove one universal memory design.

### Immutable application event journal

Use an append-only **application event table** as the source of truth. SQLite/Postgres WAL is database recovery machinery; it is not the domain event journal. An event should include:

- `event_id`, `run_id`, monotonic `seq`, `event_type`, and `schema_version`;
- timestamp plus causation/correlation identifiers;
- idempotency key and optional expected revision;
- typed metadata plus inline payload or `artifact_ref`;
- an integrity checksum when audit requirements justify it.

Enforce a unique `(run_id, seq)` and append with optimistic expected revision. In the same database transaction, add any summary/embedding/index job to an outbox and update cheap synchronous projections. Workers claim jobs idempotently. Retrieval must merge derived-index results with recent not-yet-indexed events so asynchronous indexing never hides fresh memory.

Provider-side state is an optimization, never canonical state. OpenAI can persist Conversations and chain responses with `previous_response_id`, but prior inputs remain billable and retention semantics are provider-specific ([OpenAI conversation state](https://developers.openai.com/api/docs/guides/conversation-state)). Store those provider identifiers on local events so a request can resume efficiently, while retaining enough local history to replay, migrate provider, audit, and recover.

### Snapshots and compaction

Snapshots accelerate replay but do not replace events. Store the run ID, covered last sequence, reducer/schema version, checksum, and creation time. Trigger snapshots from measured replay time, event count, or accumulated bytes. Verify that replay from zero and from snapshot-plus-tail produce identical state.

Summaries are lossy derived data. Record the covered sequence range, source hashes, summarizer model/version, prompt/version, token count, and creation time. Permit multiple granularities such as turn, session, task, and project. Mark summaries stale when their source range changes or facts are superseded. Never delete the source transcript merely because a summary exists; retention/erasure is a separate policy.

### Bounded context compiler

The context compiler should be a deep module with an explicit token budget and deterministic precedence:

1. Trusted system/developer policy and tool schema.
2. Current user request and active run state.
3. Recent verbatim events needed for conversational continuity.
4. Retrieved facts/episodes with source markers and trust classification.
5. Summaries used only where verbatim history no longer fits.

Treat retrieved text as data, not higher-trust instructions. Long-term writes should be privileged operations with namespace, provenance, author/model, source event IDs, confidence, validity/supersession, review/tombstone/expiry, size limits, and secret scanning. Anthropic explicitly warns that prompt injection can persist malicious content into agent memory, illustrating why trust and provenance cannot be left to the prompt alone ([Anthropic managed-agent memory](https://platform.claude.com/docs/en/managed-agents/memory)).

### Retrieval policy

Start with hybrid-friendly semantics even if MVP implements only lexical retrieval:

1. Hard scope and access filters: user/tenant, project, repository, session, validity time.
2. FTS/BM25 for exact names, paths, identifiers, error messages, and facts.
3. Optional dense retrieval for paraphrase/conceptual similarity.
4. Recency and importance weighting.
5. Optional reranking/diversification.
6. Token-budget-aware packing with provenance.

SQLite FTS5 provides full-text indexing and built-in BM25 ranking ([SQLite FTS5](https://www.sqlite.org/fts5.html)). Start there. If evaluation shows a dense-retrieval benefit, persist model/provider/version, dimension, metric, chunker version, content hash, source event IDs, timestamp, and access scope with each embedding. Re-embedding is a versioned migration; keep the old index until the new one passes quality checks.

Approximate nearest-neighbor indexes trade recall for speed. `pgvector` performs exact search by default and documents HNSW/IVFFlat tradeoffs and filtered-ANN caveats, making it suitable for an exact baseline ([pgvector](https://github.com/pgvector/pgvector)). Evaluate retrieval recall and final task correctness separately using project traces and memory benchmarks such as LongMemEval’s extraction, cross-session, temporal, update, and abstention categories ([LongMemEval](https://proceedings.iclr.cc/paper_files/paper/2025/file/d813d324dbf0598bbdc9c8e79740ed01-Paper-Conference.pdf)).

### Caches

Use byte-bounded caches whose eviction is always correct:

- per-run reconstructed state and compiled context fragments;
- process-wide immutable records and parsed schemas;
- persistent materialized projections;
- provider prompt caches through stable prefixes where supported.

Every key must include the semantic versions that affect output: scope, source revision/hash, model, prompt, chunker, retriever, schema, and access policy. Observe hit/miss/eviction rates, retained bytes, and stale-result incidents. A cache must be deletable without losing canonical state.

## Storage choices

### SQLite WAL + FTS5: recommended MVP

SQLite WAL allows readers and a writer to operate concurrently, but there is still only one writer at a time. WAL relies on same-host shared memory and is not a network-filesystem design. Automatic checkpoints normally begin around 1,000 pages, and a long-lived reader can prevent checkpoint completion and let the WAL grow; durability behavior also varies with synchronous/checkpoint policy ([SQLite WAL](https://www.sqlite.org/wal.html), [SQLite isolation](https://sqlite.org/isolation.html)).

Use one dedicated storage writer actor, short transactions, bounded read connections, and explicit busy/admission behavior. Observe WAL bytes, checkpoint duration/progress, longest read transaction, writer queue time, and durable-append latency. Put large tool output in the artifact store. Use SQLite’s backup API or a documented checkpoint-aware procedure rather than blindly copying files ([SQLite backup API](https://sqlite.org/backup.html)).

This design is an excellent local/single-daemon starting point, not a promise of infinite write scale. Stay on SQLite while the writer queue meets the budget; dataset size alone is not the migration signal.

### PostgreSQL + pgvector: shared/multi-host scale path

PostgreSQL provides MVCC, WAL recovery, JSONB, full-text search, and concurrent multi-client service operation ([PostgreSQL MVCC](https://www.postgresql.org/docs/18/mvcc-intro.html), [PostgreSQL WAL](https://www.postgresql.org/docs/18/wal-intro.html), [JSONB](https://www.postgresql.org/docs/18/datatype-json.html), [full-text search](https://www.postgresql.org/docs/18/textsearch.html)). Migrate when multiple processes/hosts need authoritative writes, tenancy/replication/backup requirements demand it, or SQLite’s measured writer queue violates the SLO. `pgvector` then keeps exact and approximate vector retrieval beside transactional metadata, but the service/network/operations cost is real.

### LanceDB and Qdrant: optional derived retrieval

LanceDB is an embedded/serverless vector database built on the Lance columnar format with a Rust SDK and Arrow integration; its local read-consistency setting controls how aggressively it checks other-process updates ([LanceDB repository](https://github.com/lancedb/lancedb), [Lance format](https://lance.org/format/file/), [LanceDB Rust SDK](https://docs.rs/lancedb/latest/lancedb/), [read consistency](https://lancedb.github.io/lancedb/python/python/)). It is a plausible embedded, rebuildable vector/multimodal projection when FTS is insufficient. Do not require atomicity between SQLite truth and LanceDB; use the outbox.

Qdrant is a Rust vector database with dense/sparse/multivector search, payload filtering, HNSW, WAL/snapshots, sharding, and replication. Its memory tiers explicitly trade RAM for disk I/O and latency ([Qdrant overview](https://qdrant.tech/documentation/overview/), [Qdrant storage](https://qdrant.tech/documentation/manage-data/storage/), [Qdrant memory tiers](https://qdrant.tech/documentation/ops-configuration/memory-tiers/)). Choose it only when filtered vector search is itself a service-scale workload that justifies another operational boundary.

### Arrow: interchange, not the source of truth

Arrow’s columnar `RecordBatch` and IPC formats are valuable at vector/analytics batch boundaries; reading can be zero-copy when the source supports it, with documented exceptions such as compression and transformation ([Arrow Rust IPC](https://arrow.apache.org/rust/arrow_ipc/index.html), [Arrow IPC](https://arrow.apache.org/docs/cpp/ipc.html), [Rust `RecordBatch`](https://arrow.apache.org/rust/arrow/record_batch/index.html)). Arrow is not an event log, transaction manager, or retrieval policy. Do not introduce it into ordinary chat-event paths without a measured batch workload.

## Provider adapters: Rust’s main product risk

Define a provider port around harness semantics rather than the union of all provider APIs:

```text
capabilities(model) → CapabilitySet
start(Request, Deadline, Cancellation) → Stream<ProviderEvent>
cancel(provider_request_id) → best-effort outcome
count_or_estimate_tokens(request) → estimate + provenance
```

Preserve raw provider request IDs, response fields, unknown events, and usage metadata beside normalized events. Keep wire DTOs in one adapter crate per provider. Maintain recorded fixture corpora for normal streams, tool calls, structured output, unknown events, malformed/truncated streams, retries, rate limits, and cancellation. Run opt-in live contract tests against a small model, and track “provider release → supported/tested” lag as a product metric.

OpenAI publishes an official OpenAPI 3.1 description that can help generate or check wire types, but its generated official SDK set still omits Rust ([OpenAI OpenAPI](https://github.com/openai/openai-openapi), [OpenAI libraries](https://developers.openai.com/api/docs/libraries)). Google states that direct REST/gRPC is the language-agnostic path for Rust and bleeding-edge features, while acknowledging that the caller must implement authentication, types, retries, and helpers ([Google partner integration](https://ai.google.dev/gemini-api/docs/partner-integration)). This validates raw HTTP as a path, not as free maintenance.

Use a sidecar running an official TypeScript or Go SDK only when a fast-moving feature would otherwise miss a business deadline. Keep it behind the same provider port. The sidecar brings IPC, deployment, failure, and tracing costs and should be an escape hatch, not the default.

## MCP and tools

### MCP

The latest MCP stable release dated 2026-07-28 made core requests stateless and self-describing, with transport, caching, task, and security changes ([MCP 2026-07-28 release](https://blog.modelcontextprotocol.io/posts/2026-07-28/), [release-candidate details](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/)). As of this research date, the current SDK page lists Rust as Tier 1 and the official Rust SDK roadmap says all Tier 1 requirements are met, with full conformance across the dated suites ([current MCP SDK page](https://modelcontextprotocol.io/docs/2026-07-28/sdk), [Rust SDK roadmap](https://github.com/modelcontextprotocol/rust-sdk/blob/main/ROADMAP.md), [Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)). Older release-day pages and search snippets that call Rust beta or Tier 2 are stale relative to those current sources. Pin a tested SDK/protocol version and run official conformance rather than trusting a cached label alone.

The 2026-07-28 transports are newline-delimited JSON-RPC on stdio and per-request HTTP POST returning JSON or request-scoped SSE. Cancellation is cooperative and transport-specific; Streamable HTTP servers must validate `Origin`, should bind locally for local service, and need authentication remotely ([MCP transports](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports), [MCP cancellation](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/cancellation), [Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)). Put the official Rust SDK behind an internal MCP adapter so protocol revisions do not leak through the domain.

### Tool execution and process control

Rust’s `Command::arg` passes literal arguments without normal shell expansion, but Windows batch/cmd parsing is a documented exception and child processes inherit environment variables unless explicitly cleared ([Rust `Command`](https://doc.rust-lang.org/std/process/struct.Command.html)). Dropping a standard `Child` does not stop or reap it; Tokio processes also continue by default unless configured, and even `kill_on_drop` does not replace an explicit reap path ([Rust `Child`](https://doc.rust-lang.org/std/process/struct.Child.html), [Tokio process](https://docs.rs/tokio/latest/tokio/process/struct.Command.html)).

The tool supervisor should:

- resolve an allow-listed executable; never construct `sh -c` or `cmd /c` from model text;
- pass an argument array; clear and reconstruct a minimal environment with scoped credentials;
- use a validated working directory and allowed filesystem roots;
- concurrently drain stdout and stderr into byte-bounded buffers/artifacts;
- put Unix tools in their own process group and Windows tools in a Job Object;
- enforce wall, idle, output, process-count, CPU, and memory limits where supported;
- on cancellation, close stdin, request graceful exit, terminate the group/job, and reap;
- keep the authorization policy outside the prompt: the model requests, policy decides;
- record the effective sandbox capabilities in the event log and UI.

Process control is not a complete sandbox. Linux seccomp only reduces syscall surface and explicitly says it is not a sandbox by itself; Landlock provides unprivileged restriction with ABI-dependent capabilities. Windows Job Objects manage process trees/resources, while AppContainer supplies an isolation boundary ([Linux seccomp](https://kernel.org/doc/html/latest/userspace-api/seccomp_filter.html), [Landlock](https://www.kernel.org/doc/html/latest/userspace-api/landlock.html), [Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects), [AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation)). Implement sandboxing as platform adapters and use a disposable container/VM runner for the highest-risk code.

## Plugins, FFI, and WASI

Do not make native Rust dynamic libraries the ecosystem plugin mechanism. The Rust Reference states that the Rust ABI offers no stability guarantees, and FFI declarations are `unsafe` because the compiler cannot verify the foreign implementation ([Rust ABI](https://doc.rust-lang.org/nightly/reference/items/external-blocks.html), [Rustonomicon FFI](https://doc.rust-lang.org/nomicon/ffi.html)). Native plugins also share the daemon’s address space, crash domain, and authority.

Use three trust tiers:

1. Trusted built-ins compiled as workspace crates.
2. Ecosystem extensions out of process over MCP (or a narrower tool protocol).
3. Sandboxed local extensions as WASI components once a stable capability API exists.

WIT defines language-neutral component interfaces, and a component world declares its imports/exports; absent host-granted imports, the component has no ambient access through the component model ([WIT](https://component-model.bytecodealliance.org/design/wit.html), [component worlds](https://component-model.bytecodealliance.org/design/worlds.html), [components](https://component-model.bytecodealliance.org/design/components.html)). WASI 0.3 added native async support in 2026, but Rust component toolchains still have maturity/tier caveats, so this should not block MVP ([Component Model FAQ](https://component-model.bytecodealliance.org/reference/faq.html), [Rust components](https://component-model.bytecodealliance.org/language-support/creating-runnable-components/rust.html)).

## Observability

Use `tracing` as the internal interface and an OTLP exporter as an adapter. `tracing` models structured events and spans for async systems, but its docs warn against holding an entered span guard across `.await`; use instrumentation APIs instead ([`tracing` events](https://docs.rs/tracing/latest/tracing/struct.Event.html), [`tracing` spans](https://docs.rs/tracing/latest/tracing/span/index.html)). OpenTelemetry’s current language-status table lists Rust signals as Beta, less mature than several alternatives, so pin exporter versions and avoid leaking OTel types into domain modules ([OpenTelemetry status](https://opentelemetry.io/status/), [OpenTelemetry Rust](https://github.com/open-telemetry/opentelemetry-rust)).

Trace these boundaries with one monotonic clock and correlation ID:

- UI/IPC admission and queue wait;
- state load, event append, transaction commit, snapshot, WAL checkpoint;
- context compilation, FTS/vector retrieval, embedding, reranking;
- provider DNS/connect/TLS/request write, TTFT, inter-event gaps, completion, usage;
- provider event received → normalized event → subscriber visibility;
- tool approval wait, queue wait, spawn, output, termination, reap;
- cancellation accepted → provider/task/process stopped;
- subscriber depth, coalesced text bytes, disconnect/replay;
- RSS, allocator totals if instrumented, file descriptors/handles, processes, task counts.

Default telemetry to metadata, timings, sizes, identifiers, and redacted errors. Prompt, tool argument/result, memory, and artifact content can contain secrets or personal data and must be opt-in under an explicit retention policy.

## Security model

Rust removes a large class of memory-safety defects in safe code; it does not authorize a tool, validate an untrusted prompt, protect a secret, constrain a child process, or make `unsafe` dependencies correct. The security boundary must include:

- authenticated local/remote client transports, origin checks, and loopback-by-default binding;
- least-privilege tool capabilities and user approval policy;
- tenant/project/repository scope enforced before retrieval, never after prompt assembly;
- secret references/leases instead of copying long-lived credentials into prompts or child environments;
- payload, frame, decompression, artifact, and output limits;
- signed/reproducible releases, lockfile/dependency audit, and an `unsafe`/native dependency inventory;
- tamper-evident event checksums where audit requirements demand them;
- retention, erasure, export, and encryption-at-rest policy for canonical history and artifacts.

Remote web UI support expands the threat model materially. Treat it as a later adapter with CSRF/origin/auth/session hardening, not as a harmless transport swap.

## Testing strategy

Rust’s type system helps, but agent-harness correctness is primarily a state, failure, and compatibility problem. Build the following test layers:

- Pure reducer unit tests and golden replay tests.
- Property tests: monotonic sequence, exactly one terminal outcome, no output after terminal, tool result matches a call, snapshot+tail equals full replay, idempotent retries.
- Provider fixture tests for every captured SSE/WS sequence, including unknown, fragmented, malformed, reordered where legal, truncated, retried, and cancelled streams.
- Parser fuzzing for JSON, SSE, MCP, client protocol, artifact metadata, and path handling. `cargo-fuzz` is the established libFuzzer path in the Rust Fuzz Book ([Rust Fuzz Book](https://rust-fuzz.github.io/book/)).
- Deterministic time tests for timeouts/retry/backoff using Tokio’s test-time support ([Tokio testing](https://tokio.rs/tokio/topics/testing)).
- Loom tests for small custom concurrency primitives; Loom explores possible concurrent executions but has documented model limits, so it supplements rather than proves concurrency correctness ([Loom](https://github.com/tokio-rs/loom)).
- Crash-injection between append, projection, outbox claim, snapshot, embedding write, and index publish.
- Cancellation races at every await and process transition.
- Linux/macOS/Windows CI for paths, signals, process trees, encodings, packaging, and sandbox capability reporting.
- Multi-hour soak and overload tests for RSS plateau, task/handle/file-descriptor leaks, zombies, queue bounds, reconnect/replay, and graceful shutdown.

Provider live tests must be separate from deterministic offline CI because external availability, model behavior, and billing are not stable test dependencies.

## Cross-platform packaging and engineering cost

Rust’s Tier 1 targets include mainstream Windows, macOS, and Linux combinations with official builds/tests, but cross-compilation can still require platform SDKs, linkers, native libraries, signing, and notarization ([Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html), [rustup cross-compilation](https://rust-lang.github.io/rustup/cross-compilation.html)). Build and sign on native CI runners initially. Prefer a daemon and CLI release first; ship web/desktop UI on its independent cadence.

Rust’s real costs are the ownership/async learning curve, slower compile-link cycles, generics/error-type complexity, native dependency friction, and less first-party AI-provider coverage. Cargo supplies build timing reports, incremental caches, profiles, workspace sharing, and build-performance guidance, but these are optimization tools rather than evidence that compile time will be trivial ([Cargo timings](https://doc.rust-lang.org/cargo/reference/timings.html), [Cargo build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html), [Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html), [Cargo build performance](https://doc.rust-lang.org/cargo/guide/build-performance.html)). Keep crates aligned to genuine change boundaries, minimize feature explosions and proc-macro-heavy dependencies, and monitor clean/incremental build critical paths from the first milestone.

## Rust versus alternatives

| Criterion | Rust | Go | TypeScript/Node | Python | Rust-core hybrid |
|---|---|---|---|---|---|
| Harness runtime/resource control | Excellent, explicit ownership, no GC | Excellent; simpler concurrency conventions, GC | Good for I/O; event-loop blocking must be controlled | Good for I/O; CPU/runtime packaging need care | Excellent core; complexity moves to protocol boundary |
| Provider day-zero support | **Weakest** for OpenAI/Anthropic/Gemini | Strong; official provider SDK coverage | Strongest web/provider ecosystem | Strong provider and ML ecosystem | Sidecars can close selective gaps |
| Native process/sandbox integration | Excellent | Very good | Adequate through OS APIs/packages | Adequate through OS APIs/packages | Rust owns trusted execution boundary |
| Single-binary deployment | Excellent | Excellent | Requires runtime/bundling | Requires interpreter/bundling | Core single binary; UI independent |
| Iteration speed for new ML/retrieval ideas | Lowest initially | Medium | High | Highest | Rust contracts + Python experiment worker |
| Compile/build feedback | Slowest of these in a large generic workspace | Fast | Fast/transpiled | Immediate | Costs concentrated in core |
| MCP | Official Rust SDK; current Tier 1 roadmap | Official Tier 1 | Official Tier 1 | Official Tier 1 | Rust core owns MCP |
| Best reason to choose | Safety, resource bounds, native core, learning goal | Fastest balanced product path | Provider/UI velocity | Research iteration | Durable core plus ecosystem escape hatches |

Go goroutines are lightweight and multiplexed by the runtime, Go uses garbage collection, and `context.Context` is the standard hierarchical deadline/cancellation convention ([Go FAQ](https://go.dev/doc/faq), [Go GC guide](https://go.dev/doc/gc-guide), [Go context](https://go.dev/blog/context)). Given official provider SDKs and easier iteration, **Go is the strongest alternative** if time-to-feature-parity outweighs the Rust learning goal and fine-grained resource control.

Node is well suited to I/O orchestration, but its own guidance notes that long callbacks block other clients on the event loop ([Node event-loop guidance](https://nodejs.org/en/learn/asynchronous-work/dont-block-the-event-loop)). A competent TypeScript implementation may already meet the latency budget because network/model time dominates; do not justify a rewrite without measurement. TypeScript remains a natural web/desktop frontend language and a useful adapter sidecar for provider betas.

Traditional CPython permits one thread to execute Python bytecode at once, while I/O releases the GIL and free-threaded builds are now optional ([Python glossary](https://docs.python.org/3/glossary.html)). Python remains the best place to prototype retrieval, evaluation, and ML-heavy transforms. Use a process contract instead of Python/Rust FFI so crashes, dependencies, the GIL, and packaging remain outside the trusted core.

The preferred hybrid is **Rust core daemon + TypeScript UI + optional Python evaluation/experimental worker**. Sidecars are selectively justified by ecosystem leverage, not as a substitute for clear core interfaces.

## Benchmark and experiment plan

No language-performance conclusion is valid until this suite runs against competent implementations. Use fixed hardware/OS/toolchains, release builds, identical request semantics and durability, interleaved/randomized trials, and p50/p95/p99 plus confidence intervals. Separate warm/cold, direct/through-harness, and harness/provider/tool time. Criterion can help with isolated statistical microbenchmarks, but the critical results are end-to-end distributions from a purpose-built harness ([Criterion](https://bheisler.github.io/criterion.rs/book/)).

### Experiment 1: deterministic local stream relay

Build a fake provider on loopback that:

- accepts the target provider request shape;
- supports immediate or configured TTFT;
- emits a known SSE byte/event sequence at a fixed cadence;
- fragments JSON/SSE at arbitrary byte boundaries;
- emits tiny and large chunks, unknown event kinds, errors, disconnects, and retry hints;
- pauses reads to create outbound backpressure;
- records monotonic timestamps at socket boundaries.

Run direct client → fake provider as the baseline, then client → harness → fake provider. Measure incremental overhead in first byte, every relayed event, completion, CPU time, allocations, RSS, queue wait/depth, coalesced/dropped bytes, and cancellation. Run one, target, and overload concurrency. Compare the Rust spike with a competent Go or TypeScript spike using the same wire corpus and contract.

### Experiment 2: event persistence and recovery

Generate realistic histories at 100, 1k, 10k, and 100k events with representative payload/artifact distributions. For each SQLite durability/checkpoint policy, measure append p50/p95/p99, batches, database/WAL growth, checkpoint time, snapshot creation, replay from zero, snapshot+tail recovery, and forced-kill recovery. Inject process death before/after append, projection, outbox claim, snapshot publish, and final acknowledgement. Gates: no lost acknowledged events, no duplicate logical effects, monotonic revision, and identical replay state.

### Experiment 3: retrieval quality and performance

Create representative corpora at 10k, 100k, and 1m chunks. Establish labeled relevance and exact-search baselines. Compare FTS5, hybrid FTS+dense, and only then pgvector/LanceDB/Qdrant using recall@k, MRR/nDCG where appropriate, answer correctness, abstention, update/supersession accuracy, p50/p95/p99 warm/cold latency, index freshness, build/rebuild time, RSS, disk, and filtered recall at several selectivities. Reject a vector dependency if it does not improve project-task quality enough to justify its new seam.

### Experiment 4: context and memory quality

Use LongMemEval categories plus project traces covering exact file/error recall, cross-session decisions, temporal ordering, updated/contradicted facts, recurring tool failures, permissions/scope isolation, and absence/abstention. Compare recent raw history, summary-only, FTS retrieval, hybrid retrieval, and hierarchical summaries. Report retriever recall separately from final model answer quality.

### Experiment 5: overload and resource safety

Drive offered load above configured capacity for at least 30 minutes, then soak at target load for hours. Verify that every queue stays within its configured bound, RSS plateaus, admissions follow policy, slow subscribers disconnect/replay predictably, cancellation returns permits, stdout/stderr cannot grow unbounded, no child remains unreaped, and graceful shutdown reports unfinished work. Repeat with a provider that stops reading and a UI that stops consuming.

### Experiment 6: provider-maintenance cost

During the MVP, implement the same narrow feature set in Rust raw HTTP and, for one provider, its official Go or TypeScript SDK. Track engineering hours, code/fixture volume, release lag, schema breakages, retries/auth complexity, and live-contract failures. This tests Rust’s largest risk instead of treating it as an opinion.

### Adoption gates

Mandatory correctness gates:

- Zero lost acknowledged events and zero duplicate logical effects in the fault suite.
- Full replay and snapshot+tail yield identical state.
- Zero cross-scope retrieval leakage.
- Enforceable bounds on all queues, caches, bodies, outputs, artifacts, and concurrency.
- RSS reaches a stable plateau under the defined overload/soak test.
- Reconnect from a sequence cursor reconstructs identical client-visible state.
- Cancellation leaves no owned tasks, permits, transactions, or child processes.

Suggested performance gates (targets, not observed facts):

- Meet the latency table above on the reference host.
- Rust should show either a meaningful owned-stage win—start with ≥20% lower owned-stage p95 latency or ≥30% lower CPU/RSS at target concurrency—or independently justify itself through deployment, safety, native supervision, and the explicit learning objective.
- If Rust and a simpler alternative both keep owned overhead under budget, choose based on maintainability, provider velocity, and team competence rather than benchmark theater.

Provider/product gates:

- The three priority providers pass recorded and live stream/tool/cancel contract tests.
- A documented SLA exists for new provider features and schema incidents.
- One developer can add a provider without changing domain, memory, protocol, or UI modules.
- ACP/custom-protocol spike resolves replay, multi-client, artifact, approval, and versioning semantics.
- Linux/macOS/Windows process lifecycle tests pass; hardened sandbox claims are capability-specific, never a single boolean.

## Phased implementation plan

### Phase 0 — two-week risk spike

- Pure domain reducer with command/event envelopes and replay.
- Typed in-process adapter plus minimal JSONL process adapter.
- Fake SSE provider and full timestamping benchmark.
- One real provider via raw HTTP, with golden stream fixtures.
- SQLite event append/replay with forced-kill test.
- ACP mapping spike against the same commands/events.
- Small Go or TypeScript comparison for stream relay and provider maintenance.

**Exit:** architecture and language pass the adoption gates or the project consciously chooses Rust for safety/deployment/learning despite no measured speed advantage.

### Phase 1 — usable local MVP

- Rust daemon and CLI client; TUI can use either in-process or process adapter.
- Per-run actor, bounded queues/semaphores, hierarchical cancellation, graceful shutdown.
- OpenAI plus one other priority provider; Ollama as a local provider adapter if needed.
- Immutable SQLite event journal, snapshots, artifact store, and FTS5.
- Supervised process tools with approval, environment/cwd limits, bounded output, termination/reap.
- MCP adapter pinned to a tested release.
- Structured tracing and stage latency metrics.
- No ANN, WASI plugin runtime, multi-host store, or embedded inference engine.

### Phase 2 — client and memory maturity

- Desktop/web/TUI adapters over the same versioned contract.
- Sequence replay, multi-client observation, schema negotiation, and protocol compatibility suite.
- Context compiler, versioned summaries, outbox workers, long-term memory review/expiry.
- Retrieval evaluation corpus; add embeddings only if it clears a quality gate.
- Provider sidecar only for a demonstrated feature-lag incident.
- Platform sandbox adapters and effective-capability UI.

### Phase 3 — scale and ecosystem, only when triggered

- PostgreSQL migration for multi-host/concurrent-authoritative-writer requirements.
- LanceDB/Qdrant/pgvector selected by measured workload, not fashion.
- WASI component host after a stable WIT capability API and security review.
- Remote authenticated gateway, tenancy, encryption/retention operations.
- Native Rust inference evaluation only as a separate provider adapter project.

## Risks and mitigations

| Risk | Likelihood / impact | Mitigation and trigger |
|---|---|---|
| Provider API/SDK lag | High / high | Isolated adapters, raw fixtures, live contract tests, OpenAPI/proto checks, optional official-SDK sidecar, lag SLA. |
| Rust learning slows MVP | High / medium-high | Phase-0 timebox, simple ownership/actor patterns, avoid clever lifetimes/generics, compare delivery data. |
| Async cancellation leaks work | Medium / high | Task supervision tree, explicit deadlines, child process groups/jobs, race tests, shutdown report. |
| “Zero-copy” complexity harms locality | Medium / medium | Owned domain events by default; borrow/share only at profiled boundaries. |
| SQLite writer/checkpoint stalls | Medium / medium-high | Single writer, short transactions, WAL metrics, checkpoint tests; Postgres migration gate. |
| Retrieval adds complexity without quality | High / medium | FTS5 first, labeled evals, exact baseline, derived index/outbox, deletion test. |
| Durable memory becomes prompt-injection channel | Medium / high | Provenance/trust labels, privileged writes, scope before retrieval, review/tombstone, never elevate retrieved text. |
| Native plugins compromise core | Medium / high | No Rust dylib ABI; MCP processes first, WASI later. |
| Frontend protocol becomes accidental internal dump | Medium / high | Small versioned commands/events, compatibility fixtures, raw provider metadata behind explicit escape hatch. |
| Cross-platform sandbox overclaims | High / high | Capability report per platform; external container/VM for high-risk execution. |
| Compile time erodes iteration | Medium / medium | Genuine crate boundaries, dependency/feature discipline, Cargo timing budget, native CI cache. |

## Final go/no-go recommendation

**Go with Rust** if the product is intended to be a durable local/edge orchestration engine, the team accepts provider adapter ownership, and the Phase-0 spike meets correctness plus overhead budgets. This choice is justified by resource predictability, native process control, deployment, and the learning objective even if the end-to-end speedup is modest.

**Choose Go instead** if the overriding objective is fastest broad provider feature parity with a lower language-learning cost. It is the strongest single-language alternative today because it combines efficient native orchestration with broader official provider SDK coverage.

**Choose TypeScript for the first prototype** if requirements and provider surfaces are still changing faster than architecture can stabilize. Keep the command/event contract clean so a measured Rust core can replace the implementation later.

The most robust conclusion is not “Rust makes the model fast.” It is: **Rust can make the harness-owned path small, bounded, observable, and reliable, while a deep-module architecture keeps UIs, providers, memory indexes, tools, and inference runtimes replaceable.** The benchmark plan—not language reputation—must decide whether that path is negligible for this product.

## Primary-source bibliography

All sources were accessed 2026-09-28. Vendor and project documentation is first-party; research entries are original papers/proceedings.

### Rust, Tokio, serialization, build, and testing

- Rust Project: [Ownership](https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html), [ownership mechanics](https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html), [unsafe Rust](https://doc.rust-lang.org/book/ch19-01-unsafe-rust.html), [Rust ABI](https://doc.rust-lang.org/nightly/reference/items/external-blocks.html), [FFI](https://doc.rust-lang.org/nomicon/ffi.html), [`Command`](https://doc.rust-lang.org/std/process/struct.Command.html), [`Child`](https://doc.rust-lang.org/std/process/struct.Child.html), [platform support](https://doc.rust-lang.org/rustc/platform-support.html).
- Tokio: [channels](https://tokio.rs/tokio/tutorial/channels), [`select!`](https://tokio.rs/tokio/tutorial/select), [runtime](https://docs.rs/tokio/latest/tokio/runtime/), [`spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html), [processes](https://docs.rs/tokio/latest/tokio/process/struct.Command.html), [testing](https://tokio.rs/tokio/topics/testing), [CancellationToken](https://docs.rs/tokio-util/latest/tokio_util/sync/index.html).
- Serde: [deserializer lifetimes](https://serde.rs/lifetimes.html); serde_json: [`RawValue`](https://docs.rs/serde_json/latest/serde_json/value/struct.RawValue.html); bytes: [`Bytes`](https://docs.rs/bytes/latest/bytes/struct.Bytes.html).
- Cargo: [timings](https://doc.rust-lang.org/cargo/reference/timings.html), [build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html), [profiles](https://doc.rust-lang.org/cargo/reference/profiles.html), [build performance](https://doc.rust-lang.org/cargo/guide/build-performance.html).
- Tokio project: [Loom](https://github.com/tokio-rs/loom); Rust Fuzz project: [Rust Fuzz Book](https://rust-fuzz.github.io/book/); Criterion project: [Criterion.rs](https://bheisler.github.io/criterion.rs/book/).

### Providers, inference, and harness precedents

- OpenAI: [libraries](https://developers.openai.com/api/docs/libraries), [OpenAPI](https://github.com/openai/openai-openapi), [streaming](https://developers.openai.com/api/docs/guides/streaming-responses), [latency optimization](https://developers.openai.com/api/docs/guides/latency-optimization), [production practices](https://developers.openai.com/api/docs/guides/production-best-practices), [conversation state](https://developers.openai.com/api/docs/guides/conversation-state), [Codex repository](https://github.com/openai/codex), [Codex app-server client](https://github.com/openai/codex/blob/main/codex-rs/app-server-client/README.md).
- Anthropic: [SDKs](https://platform.claude.com/docs/en/cli-sdks-libraries/overview), [streaming](https://platform.claude.com/docs/en/build-with-claude/streaming), [versioning](https://platform.claude.com/docs/en/api/versioning), [memory tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool).
- Google: [GenAI libraries](https://ai.google.dev/gemini-api/docs/libraries), [partner integration](https://ai.google.dev/gemini-api/docs/partner-integration), [generate-content API](https://ai.google.dev/api/generate-content), [Live API](https://ai.google.dev/api/live).
- AWS: [AWS SDK for Rust](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/welcome.html), [Bedrock Rust examples](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/rust_bedrock-runtime_code_examples.html).
- Ollama: [API introduction](https://docs.ollama.com/api/introduction), [streaming](https://docs.ollama.com/api/streaming), [generation metrics](https://docs.ollama.com/api/generate); llama.cpp: [server](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md); vLLM: [online serving](https://docs.vllm.ai/en/latest/serving/openai_compatible_server/).
- AAIF Goose: [repository](https://github.com/aaif-goose/goose), [custom distributions and ACP clients](https://github.com/aaif-goose/goose/blob/main/CUSTOM_DISTROS.md), [client/server roadmap](https://github.com/aaif-goose/goose/discussions/7697).

### Memory, storage, retrieval, and research

- SQLite: [WAL](https://www.sqlite.org/wal.html), [isolation](https://sqlite.org/isolation.html), [FTS5](https://www.sqlite.org/fts5.html), [backup](https://sqlite.org/backup.html).
- PostgreSQL: [MVCC](https://www.postgresql.org/docs/18/mvcc-intro.html), [WAL](https://www.postgresql.org/docs/18/wal-intro.html), [JSON](https://www.postgresql.org/docs/18/datatype-json.html), [full-text search](https://www.postgresql.org/docs/18/textsearch.html); pgvector: [repository and index semantics](https://github.com/pgvector/pgvector).
- Apache Arrow: [Rust IPC](https://arrow.apache.org/rust/arrow_ipc/index.html), [IPC](https://arrow.apache.org/docs/cpp/ipc.html), [`RecordBatch`](https://arrow.apache.org/rust/arrow/record_batch/index.html).
- Lance/LanceDB: [LanceDB](https://github.com/lancedb/lancedb), [Lance format](https://lance.org/format/file/), [Rust SDK](https://docs.rs/lancedb/latest/lancedb/); Qdrant: [overview](https://qdrant.tech/documentation/overview/), [storage](https://qdrant.tech/documentation/manage-data/storage/), [memory tiers](https://qdrant.tech/documentation/ops-configuration/memory-tiers/).
- Research: [RAG](https://papers.neurips.cc/paper/2020/file/6b493230205f780e1bc26945df7481e5-Paper.pdf), [MemGPT](https://arxiv.org/abs/2310.08560), [Generative Agents](https://doi.org/10.1145/3586183.3606763), [Reflexion](https://papers.neurips.cc/paper_files/paper/2023/hash/1b44b878bb782e6954cd888628510e90-Abstract-Conference.html), [RAPTOR](https://openreview.net/pdf?id=GN921JHCRw), [Lost in the Middle](https://direct.mit.edu/tacl/article/doi/10.1162/tacl_a_00638/119630/Lost-in-the-Middle-How-Language-Models-Use-Long), [LongMemEval](https://proceedings.iclr.cc/paper_files/paper/2025/file/d813d324dbf0598bbdc9c8e79740ed01-Paper-Conference.pdf).

### Protocols, extensions, observability, and platform security

- MCP: [2026-07-28 release](https://blog.modelcontextprotocol.io/posts/2026-07-28/), [current SDK tiers](https://modelcontextprotocol.io/docs/2026-07-28/sdk), [transports](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports), [cancellation](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/cancellation), [official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk), [Rust SDK roadmap](https://github.com/modelcontextprotocol/rust-sdk/blob/main/ROADMAP.md).
- ACP: [official organization and specification repository](https://github.com/agentclientprotocol), [official Rust SDK](https://github.com/agentclientprotocol/rust-sdk), [official TypeScript SDK](https://github.com/agentclientprotocol/typescript-sdk).
- Bytecode Alliance: [WIT](https://component-model.bytecodealliance.org/design/wit.html), [components](https://component-model.bytecodealliance.org/design/components.html), [worlds](https://component-model.bytecodealliance.org/design/worlds.html), [FAQ](https://component-model.bytecodealliance.org/reference/faq.html).
- Tokio ecosystem: [`tracing`](https://docs.rs/tracing/latest/tracing/); OpenTelemetry: [status](https://opentelemetry.io/status/), [Rust implementation](https://github.com/open-telemetry/opentelemetry-rust).
- Linux kernel: [seccomp](https://kernel.org/doc/html/latest/userspace-api/seccomp_filter.html), [Landlock](https://www.kernel.org/doc/html/latest/userspace-api/landlock.html); Microsoft: [Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects), [AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation).
- Go Project: [FAQ](https://go.dev/doc/faq), [GC guide](https://go.dev/doc/gc-guide), [context](https://go.dev/blog/context); Node.js: [event-loop guidance](https://nodejs.org/en/learn/asynchronous-work/dont-block-the-event-loop); Python: [glossary/GIL](https://docs.python.org/3/glossary.html).
