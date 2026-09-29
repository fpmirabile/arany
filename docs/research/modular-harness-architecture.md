# Modular, polyglot architecture for the harness

**Status:** research and architectural recommendation  
**Research date:** 2026-09-28  
**Question:** How should a Rust-first agent harness be decomposed so that its core remains fast and coherent while CLI, web, desktop, IDE, providers, storage, and tool implementations can be replaced independently—even by implementations written in other languages?

> **Scope amendment — 2026-09-29:** Version 1 has one product Client: `arany`, one package, and one process. References below to multiple crates/adapters, daemon/protocol, web, desktop, IDE, TUI, SDK, HTTP, or SSE are triggered future-option analysis, not version-1 commitments. Provider is the only Engine behavior seam. Optional OTLP is a private output module after the core proof, not another adapter trait. The [closure audit](./research-closure-audit.md), [system overview](../architecture/system-overview.md), and [OTLP report](./otlp-observability.md) are canonical for current scope.

## Reading guide

This report deliberately separates evidence from design judgment:

- **Fact** means the statement is directly supported by a cited primary source: an official specification, official documentation or source repository, or an original paper/article by its author.
- **Inference** means the conclusion follows from those facts but is not itself promised by a source.
- **Recommendation** is the proposed design for this repository.
- **Gate** is a claim that should be proved by a spike, test, or measurement before it becomes a lasting decision.

The terms **module**, **interface**, **implementation**, **seam**, **adapter**, **depth**, **leverage**, and **locality** are used precisely. A module may be an in-crate Rust module, a crate, a process, or a whole tier. Its interface includes not only type signatures but also invariants, ordering, failure behavior, configuration, and performance expectations. A seam is a place where one implementation can be changed without editing the caller. An adapter connects a specific technology at a seam.

## Executive conclusion

**Recommendation:** build a Rust engine with one small in-process interface and place a versioned, language-neutral process protocol around it. Treat every user experience—Rust CLI, possible C++ CLI, web, desktop, IDE, tests, and automation—as a driving adapter. Treat providers, persistence, tools, MCP, sandboxing, and telemetry as driven adapters. Keep domain policy independent of Tokio, HTTP, SQL, provider payloads, MCP types, and UI models.

This yields the intended dependency direction:

```text
CLI / web / desktop / IDE / automation
                  │
          driving adapters
                  │
       versioned protocol seam
                  │
               engine
                  │
               domain
                  │
          required ports only
                  │
 providers / storage / tools / sandbox / telemetry
             driven adapters
```

The most important architectural decision is not “how many crates?” It is **where information is hidden and which direction source dependencies point**. Robert Martin's Dependency Rule says source dependencies point inward and inner policy must not name outer mechanisms or use their data formats. He also says the familiar four circles are schematic, not a required directory count. [[Clean Architecture](https://blog.cleancoder.com/uncle-bob/2012/08/13/the-clean-architecture.html)] **Fact**

The strongest decomposition criterion is not execution order. Parnas showed that modules should hide difficult or changeable design decisions, rather than mirror a flowchart. [[Parnas, *On the Criteria To Be Used in Decomposing Systems into Modules*](https://dl.acm.org/doi/10.1145/361598.361623)] **Fact** A harness split into `receive -> parse -> call-model -> call-tool -> persist -> render` leaks shared decisions—run state, ordering, cancellation, retries, and durability—across temporal stages. The better decomposition groups the code that owns each decision.

Rust is a good language for the engine, but Rust crates are not a polyglot ABI. The Rust Reference explicitly states that the native Rust ABI has no stability guarantees; `cdylib` exists for libraries loaded from other languages, normally through an explicitly designed C ABI. [[Rust Reference: external blocks](https://doc.rust-lang.org/reference/items/external-blocks.html#abi)] [[Rust Reference: linkage](https://doc.rust-lang.org/reference/linkage.html)] **Fact** Therefore a C++ CLI should normally speak to the Rust daemon through a process protocol, not link directly to Rust types. **Recommendation**

The OpenCode precedent supports the feasibility of replaceable clients: its official server documentation says the TUI is a client of an HTTP server, the server publishes OpenAPI, and this permits multiple clients. Its SDK documentation says types are generated from that OpenAPI document. [[OpenCode server](https://opencode.ai/docs/server/)] [[OpenCode SDK](https://opencode.ai/docs/sdk/)] **Fact** Current OpenCode source also contains separate protocol, server, generated client, TUI, app, desktop, and embedded-host packages. Its embedded host runs the same assembled HTTP router in memory without opening a listener. [[OpenCode `sdk-next` README at audited commit](https://github.com/anomalyco/opencode/blob/7f964bbb00e505178847e2c08721b0fff56208f9/packages/sdk-next/README.md)] **Fact** This is a useful precedent, not a topology to copy mechanically.

## 1. What “modular” must mean here

### 1.1 Replaceability is an outcome, not a folder count

**Recommendation:** call the harness modular only if these changes are local:

1. A new frontend is added without importing engine implementation or provider code.
2. A provider schema changes without changing domain state, client protocol, or persistence records.
3. SQLite is replaced or supplemented without changing run orchestration.
4. MCP or a native process tool is added without changing client code.
5. the CLI language changes without changing engine behavior.
6. a run can be tested deterministically through the same engine interface used by production adapters.

These are locality tests. “Every concept has a crate” does not prove any of them.

Cargo defines a crate as the smallest unit Rust compiles, and a workspace as packages managed together with a shared lockfile and target directory. Rust modules already provide namespace and privacy controls inside a crate; items are private by default, with restricted visibility such as `pub(crate)` available. [[Rust Book: packages and crates](https://doc.rust-lang.org/book/ch07-01-packages-and-crates.html)] [[Cargo: workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html)] [[Rust Reference: visibility and privacy](https://doc.rust-lang.org/reference/visibility-and-privacy.html)] **Fact**

**Inference:** an early project can enforce most information hiding with private Rust modules. A crate is warranted when it buys one or more of:

- enforceable dependency direction;
- an independently consumed or versioned interface;
- exclusion of heavy or platform-specific dependencies;
- independent release or ownership;
- compile and test locality;
- a real adapter set with distinct implementations.

If none applies, an additional crate mostly creates manifests, public types, conversion code, and build-graph complexity.

### 1.2 Small code is not the same as deep modules

The official Clean Code course outline emphasizes intention-revealing names, small expressive functions, side-effect discipline, command/query separation, exceptions, removing duplication, and incremental cleanup. [[Clean Code course outline](https://cleancoder.com/files/cleanCodeCourse.md)] **Fact** These practices help inside an implementation, but they do not decide the system's seams.

Ousterhout defines a module's interface as everything another module must know—including behavior, side effects, and usage constraints—and argues that an interface should be much simpler than its implementation. He warns that excessive small classes produce shallow modules and information leakage. [[Ousterhout, Modular Design lecture](https://web.stanford.edu/~ouster/cgi-bin/cs190-winter18/lecture.php?topic=modularDesign)] **Fact**

**Recommendation:** combine the useful parts:

- Use clear names, cohesive functions, explicit errors, tests, and continual cleanup inside each module.
- Optimize seams for depth: few concepts learned by callers, much policy hidden behind them.
- Do not turn “do one thing” into one crate, trait, or type per verb.
- Prefer one coherent run engine over a chain of `PromptBuilder`, `ToolLoopManager`, `RetryService`, `EventService`, and `PersistenceService` objects that callers must assemble in the correct order.

### 1.3 Separate by hidden decision and reason to change

Martin's clarification of the Single Responsibility Principle says a module should respond to one tightly coupled business function, drawing directly on Parnas's change-hiding criterion. [[Martin, Single Responsibility Principle](https://blog.cleancoder.com/uncle-bob/2014/05/08/SingleReponsibilityPrinciple.html)] **Fact**

For this harness, the important decisions to hide are:

| Hidden decision | Owning module | Must not leak |
|---|---|---|
| Valid run/tool/approval state transitions | Domain | Tokio tasks, HTTP, SQL rows, UI state |
| Admission, orchestration, cancellation, recovery | Engine | Provider event shapes, transport requests |
| Client compatibility and wire evolution | Protocol adapters | Rust-only enums, database schema |
| Provider authentication, request/stream mapping, retry hints | Provider adapter | OpenAI/Anthropic/etc. payload types |
| Canonical journal, snapshots, transactions | Memory/storage implementation | SQL, WAL/checkpoint details |
| Context selection and budgeting policy | Context module inside engine | Vector-database result types |
| Tool lifecycle, approval, limits, process supervision | Tool runtime | Shell strings, OS handles |
| OS containment capabilities | Sandbox adapter | Platform-specific syscalls in domain |
| Trace/export implementation | Telemetry adapter | OpenTelemetry types in domain |
| Presentation, input, navigation | Each client | Engine internals and provider SDKs |

This table is more important than the eventual directory tree.

## 2. Recommended architectural shape

### 2.1 Domain: pure policy and state transitions

**Recommendation:** the domain owns the stable vocabulary and rules:

- `Run`, `Session`, `Workspace`, `Message`, `ToolCall`, `Approval`, `Artifact`, `MemoryEntry`;
- identifiers and scope types;
- run and tool state machines;
- transition validation;
- authorization-neutral capability requirements such as “this action requires filesystem-write approval”;
- pure reducers from prior state plus accepted event to next state;
- domain errors such as invalid transition, conflict, or missing aggregate.

It must not depend on Tokio, Axum, Reqwest, SQLx, SQLite, MCP, ACP, OpenTelemetry, provider SDKs, or frontend types. This is the direct application of the inward Dependency Rule. **Recommendation**

Domain entities need not be elaborate object hierarchies. Martin explicitly permits entities to be objects with methods or data structures plus functions. [[Clean Architecture](https://blog.cleancoder.com/uncle-bob/2012/08/13/the-clean-architecture.html)] **Fact** Rust value types plus pure functions are appropriate.

### 2.2 Engine: use cases and ownership

The engine is the deep module that provides most of the repository's leverage. It owns:

- request validation after transport/auth translation;
- run admission and idempotency;
- one logical owner for each live run;
- provider/tool loop orchestration;
- bounded scheduling and backpressure;
- cancellation propagation;
- event commit and publish ordering;
- recovery and replay;
- context compilation from recent conversation, retrieved memory, policy, and tool definitions;
- retry decisions expressed in domain terms;
- coordination of approvals and tool execution;
- lifecycle and graceful shutdown.

**Recommendation:** callers ask the engine to accomplish a use case; they do not manually invoke its steps. That hides temporal coupling. The caller must not know that a turn is loaded, reduced, context-compiled, persisted, streamed, snapshotted, and indexed in that order.

The engine may use Tokio internally. “Clean” does not mean framework-free implementation; it means the framework does not define the engine's external interface or domain policy. The Tokio runtime and channels remain implementation details.

### 2.3 Driving adapters: all clients are peers

Cockburn's original Ports and Adapters formulation aims to let an application be driven equally by users, programs, automated tests, or batch scripts and to let it be developed and tested independently of external runtime devices and databases. [[Cockburn, Hexagonal Architecture](https://alistair.cockburn.us/hexagonal-architecture/)] **Fact**

Create adapters for:

- local stdio or local-socket protocol;
- HTTP request/response plus SSE event streaming for browser and automation;
- ACP for compatible IDE/editor clients;
- a Rust in-process adapter for tests and trusted embedding;
- generated or handwritten client libraries.

The CLI is not the engine. It is a presentation adapter that owns argument parsing, terminal state, keyboard input, rendering, and user interaction. A C++ CLI can spawn or discover `aranyd`, negotiate the protocol, submit requests, and consume the event stream. The same is true for a web or desktop client.

**Recommendation:** make the first real CLI exercise the public process seam, even if it is written in Rust. Otherwise the seam will remain hypothetical until the first non-Rust client exposes missing behavior.

### 2.4 Driven adapters: external capabilities

Ports belong to the engine side and use the engine's language. Adapters translate external technologies into those ports.

Examples:

- `ModelProvider`: complete or stream a normalized model exchange;
- `Journal`: atomically append accepted events and load history;
- `ArtifactStore`: put/get bounded blobs by content reference;
- `ToolExecutor`: execute an authorized tool specification with cancellation and limits;
- `MemoryIndex`: index/query derived retrieval records;
- `Sandbox`: report capabilities and spawn a constrained process;
- `Clock`, `IdSource`, and `Telemetry`: only if determinism or real variation justifies seams.

**Recommendation:** do not create a trait merely because a dependency exists. A trait earns its place when there are at least two meaningful adapters—typically a production adapter and a deterministic test adapter for a true external dependency, or two real production technologies. SQLite can often be tested as SQLite in a temporary database; wrapping every query in repository traits may reduce rather than increase depth.

## 3. Dependency categories and test strategy

### 3.1 In-process dependencies

Pure reducers, state machines, budget calculations, capability policy, and event transformations are ordinary in-process dependencies. Merge them behind the engine's internal modules; no adapter is needed.

Test through the narrowest meaningful interface. Pure domain invariants can be property-tested directly. Engine behavior should be tested through its request/event interface.

### 3.2 Local-substitutable dependencies

SQLite databases, temporary filesystems, and local artifact directories have cheap real stand-ins. **Recommendation:** use temporary real implementations in integration tests rather than exposing a wide public port solely to mock them. Keep any storage seam internal to the engine implementation unless a second persistent backend becomes a real requirement.

### 3.3 Remote but owned dependencies

If a future hosted coordinator, remote worker, or shared memory/index service is owned by this project, define a port in the calling module's vocabulary and implement network plus in-memory adapters. The network schema is an adapter detail, not a domain type.

### 3.4 True external dependencies

AI providers, MCP servers, and platform services are true external dependencies. Give each family a small internal port plus deterministic fake adapter. Provider-specific capabilities must be explicitly discoverable rather than collapsed into a false lowest common denominator.

**Recommendation:** normalize the stable 80%—messages, streamed content, tool calls, usage, finish/error state—while preserving an escape hatch for typed provider capability declarations. Do not expose raw JSON throughout the engine.

## 4. Crate and repository topology

### 4.1 Start smaller than the final diagram

Cargo workspaces coordinate multiple packages, but each package creates another compiled and semantically versioned interface. Cargo's compatibility guide classifies renaming, moving, or removing public items as breaking changes. [[Cargo SemVer compatibility](https://doc.rust-lang.org/cargo/reference/semver.html)] **Fact**

**Recommendation for the first executable architecture:**

```text
Cargo.toml
crates/
  arany-domain/         # pure vocabulary, events, reducers, invariants
  arany-engine/         # deep use-case interface and implementation
  arany-protocol/       # language-neutral wire DTOs/schema generation only
  arany-adapters/       # initial concrete providers/storage/tools/transports
bins/
  aranyd/               # composition root and daemon lifecycle
  arany-cli/            # first driving adapter; talks to aranyd
clients/
  web/                   # added only when it exists
  cpp/                   # added only if the C++ experiment wins
schemas/
  protocol/              # generated/published contract artifacts
```

`arany-protocol` must not import `arany-engine` or `arany-domain` merely to reuse convenient types. The protocol adapter translates wire DTOs into engine requests. That prevents a public wire commitment from freezing internal domain representation.

Initially, keep implementations under one `arany-adapters` crate with private modules. Split an adapter into its own crate only when heavy provider SDKs, native dependencies, platform conditionals, release cadence, licensing, or compile locality justify it. Likely later candidates are:

```text
adapters/provider-openai
adapters/provider-anthropic
adapters/storage-sqlite
adapters/tool-mcp
adapters/sandbox-linux
adapters/telemetry-otel
```

This is a forecast, not an instruction to scaffold empty crates.

### 4.2 Enforce direction mechanically

Allowed Rust package edges:

```text
arany-domain  <-  arany-engine
                         ^
                         |
arany-protocol <- protocol adapters -> arany-adapters
                         ^                    |
                         |                    |
                    aranyd (composition root)

arany-cli -> generated/process client only
```

The exact arrow drawing is less important than these constraints:

- domain depends on no workspace crate;
- engine depends on domain and port abstractions, never concrete adapters;
- wire protocol does not become domain representation;
- concrete adapters may depend inward;
- composition roots are the only places that know all implementations;
- clients do not import engine implementation.

**Recommendation:** add automated import/dependency tests. Current OpenCode contains bundle-boundary tests that verify its public browser client does not pull in core or server packages; this is a concrete precedent for testing architecture rather than only documenting it. [[OpenCode client import-boundary test](https://github.com/anomalyco/opencode/blob/7f964bbb00e505178847e2c08721b0fff56208f9/packages/client/test/import-boundaries.test.ts)] **Fact**

## 5. The polyglot seam

### 5.1 Process protocol first; FFI only with evidence

The Rust ABI is unstable, while `extern "C"` is defined to match the platform's dominant C ABI and `cdylib` is intended for libraries loaded from another language. [[Rust Reference: ABI](https://doc.rust-lang.org/reference/items/external-blocks.html#abi)] [[Rust Reference: linkage](https://doc.rust-lang.org/reference/linkage.html#link-cdylib)] **Fact**

An actual C ABI would require:

- opaque handles rather than Rust object layout;
- `#[repr(C)]` data or byte buffers;
- explicit allocator ownership and free functions;
- panic containment across FFI;
- version negotiation and symbol policy;
- callback threading and lifetime rules;
- per-platform packaging and toolchain testing;
- ABI compatibility tests.

**Inference:** this work creates more compatibility surface than a local protocol and shares the engine's failure domain with the UI. It is justified only when an embedding use case demonstrates that a separate process is unacceptable.

**Recommendation:** for a C++ CLI, ship the Rust engine as `aranyd` and speak a stable local protocol. This also supports web, desktop, IDE, scripting, and independent crash recovery. A terminal UI does not become materially faster merely by sharing address space with the engine; terminal rendering, user input, provider network latency, and token generation dominate. **Gate:** measure request admission and stream-relay overhead before choosing C++ for performance.

### 5.2 One semantic contract, multiple transports

Do not confuse a transport with the product interface. The semantic contract is requests, responses, events, ordering, errors, capabilities, and lifecycle. It may have these adapters:

| Context | Recommended transport adapter | Why |
|---|---|---|
| CLI owns one daemon child | framed JSON-RPC over stdio | no port discovery; easy supervision |
| Local multi-client | HTTP over loopback or Unix socket/named pipe, SSE for events | broad client support |
| Browser/web | HTTP plus SSE; WebSocket only if bidirectional demand proves necessary | native browser primitives |
| IDE/editor | ACP adapter over stdio | ecosystem interoperability |
| Rust embedding/tests | typed in-process interface | no serialization; deterministic tests |
| Remote deployment | authenticated HTTPS plus SSE/WebSocket | explicit network trust boundary |

JSON-RPC 2.0 is transport-agnostic and defines correlated requests/responses, notifications, and structured errors. [[JSON-RPC 2.0 specification](https://www.jsonrpc.org/specification)] **Fact** It does **not** define authentication, authorization, schema evolution, replay, backpressure, durability, or event ordering. Those remain harness responsibilities.

OpenAPI defines a language-neutral description for HTTP interfaces; OpenCode uses its OpenAPI document to generate SDK types. [[OpenAPI specification](https://spec.openapis.org/oas/)] [[OpenCode SDK](https://opencode.ai/docs/sdk/)] **Fact** Protobuf offers strong code generation and documented binary wire-evolution rules, including safe field addition and reserving deleted field numbers, but its JSON mapping and browser toolchain add complexity. [[Protocol Buffers proto3 guide](https://protobuf.dev/programming-guides/proto3/#updating)] **Fact**

**Recommendation:** spike two transports against exactly the same conformance tests:

1. JSON-RPC envelopes over stdio plus JSON Schema; and
2. HTTP/OpenAPI for commands and queries plus SSE for events.

Retain both if they serve distinct deployment modes without duplicating semantics. Do not adopt gRPC merely for theoretical throughput; token deltas should be coalesced and benchmarked first.

### 5.3 Contract rules that must exist from version one

**Recommendation:** the process contract includes:

- protocol version and capability negotiation;
- client identity and authenticated principal where applicable;
- stable request, run, session, event, causation, and correlation IDs;
- idempotency keys for mutating requests;
- per-run/aggregate monotonic sequence numbers;
- resumable event cursors;
- explicit acknowledgement semantics—“accepted” must state whether it is durable;
- explicit cancellation by operation/run identifier;
- bounded request sizes, nesting, attachment counts, and stream frame sizes;
- typed error categories plus opaque diagnostic IDs;
- unknown-field/unknown-event rules;
- graceful shutdown and detach/reconnect behavior;
- capability flags for optional features rather than client-name branching.

Do not promise a single global event order. Preserve causality and monotonic ordering within a run or aggregate. Cross-run ordering is an observation, not an invariant.

For slow consumers, never silently drop lifecycle, approval, tool-state, error, or completion events. Coalescing adjacent text deltas is acceptable if the protocol states it. Otherwise terminate the subscription with `Lagged { resume_after }`, allowing replay from a durable cursor.

## 6. ACP, MCP, and LSP: three different roles

### 6.1 ACP is a client/agent adapter

ACP explicitly standardizes communication between code editors and coding agents. Its official repository provides JSON-RPC protocol types and versioned JSON Schemas, and states that wire compatibility is determined by the negotiated protocol version and capabilities. [[ACP repository](https://github.com/agentclientprotocol/agent-client-protocol)] **Fact** Its schema covers initialization, sessions, prompts, streamed updates, tool calls, permissions, terminals, cancellation, and extension methods. [[ACP v1 schema](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/schema/v1/schema.json)] **Fact**

**Recommendation:** implement ACP as an IDE/editor driving adapter. Map ACP sessions, prompts, updates, permissions, and terminals to engine requests and events. Do not make ACP structs the domain model. ACP's product purpose and evolution are controlled outside this repository, and the harness also needs administrative queries, durable replay, memory/artifact management, and potentially multiple simultaneous frontends.

### 6.2 MCP is a tool/context extension seam

MCP is for a host/client to communicate with servers exposing tools, resources, prompts, and related capabilities. Its official transports include local stdio and remote Streamable HTTP; current official SDK documentation describes newline-delimited JSON over stdio and a transport interface around bidirectional JSON-RPC messages. [[MCP Go SDK lifecycle/transports](https://go.sdk.modelcontextprotocol.io/protocol/)] **Fact**

**Recommendation:** place MCP behind the engine's tool/context ports as a driven adapter. It is not the frontend protocol. An MCP server is an external capability with untrusted outputs and an independent lifecycle; it must not receive direct access to engine internals or canonical storage.

### 6.3 LSP is the architectural precedent

LSP standardizes JSON-RPC messages between development tools and language servers so that one language implementation can be reused across many editors. [[Official LSP overview](https://microsoft.github.io/language-server-protocol/)] **Fact**

**Inference:** the transferable lesson is “put expensive domain intelligence in one process with a stable protocol, then multiply clients.” The harness should not implement LSP itself for agent operations. ACP is the closer domain protocol; the harness's own contract covers product-specific behavior ACP does not.

### 6.4 Keep protocol roles visually separate

```text
Editor ──ACP──┐
Web ─HTTP/SSE─┼──> protocol adapters ──> engine
CLI ─JSON-RPC─┘                           │
                                         ├── provider adapters ──> model APIs
                                         ├── MCP adapter ────────> MCP servers
                                         ├── tool adapter ───────> child processes
                                         └── storage adapter ────> SQLite/artifacts
```

This avoids a common category error: ACP connects a human-facing client to an agent, while MCP connects an agent host to capabilities.

## 7. What to copy—and not copy—from OpenCode

### 7.1 Audited facts

The source observations in this section are pinned to OpenCode commit [`7f964bbb00e505178847e2c08721b0fff56208f9`](https://github.com/anomalyco/opencode/tree/7f964bbb00e505178847e2c08721b0fff56208f9), committed 2026-09-28. OpenCode evolves quickly, so paths on its development branch may change.

- The official stable documentation says `opencode` starts a TUI and server; the TUI is a client, the server exposes OpenAPI 3.1, and the arrangement supports multiple clients. [[Server docs](https://opencode.ai/docs/server/)] **Fact**
- The SDK can start server plus client or connect to an existing server, and its types are generated from the server's OpenAPI description. [[SDK docs](https://opencode.ai/docs/sdk/)] **Fact**
- The audited source has separate `schema`, `protocol`, `server`, `client`, `sdk-next`, `tui`, `app`, and `desktop` packages. [[Repository packages](https://github.com/anomalyco/opencode/tree/7f964bbb00e505178847e2c08721b0fff56208f9/packages)] **Fact**
- `protocol` assembles typed HTTP groups and middleware placement while server code supplies concrete middleware identities and handlers. [[Protocol API](https://github.com/anomalyco/opencode/blob/7f964bbb00e505178847e2c08721b0fff56208f9/packages/protocol/src/api.ts)] [[Server routes](https://github.com/anomalyco/opencode/blob/7f964bbb00e505178847e2c08721b0fff56208f9/packages/server/src/routes.ts)] **Fact**
- The event endpoint is SSE, has a bounded subscriber capacity of 256, emits a connection event, and sends heartbeats. [[Event handler](https://github.com/anomalyco/opencode/blob/7f964bbb00e505178847e2c08721b0fff56208f9/packages/server/src/handlers/event.ts)] **Fact**
- `sdk-next` assembles server routes and invokes their web handler in memory, preserving the same routing, middleware, handlers, codecs, and errors without opening a listener. [[Embedded host source](https://github.com/anomalyco/opencode/blob/7f964bbb00e505178847e2c08721b0fff56208f9/packages/sdk-next/src/opencode.ts)] **Fact**
- The desktop client runs a server sidecar, waits for readiness and health, and has bounded startup and shutdown timeouts. [[Desktop server source](https://github.com/anomalyco/opencode/blob/7f964bbb00e505178847e2c08721b0fff56208f9/packages/desktop/src/main/server.ts)] **Fact**
- OpenCode implements ACP as an adapter whose methods delegate to an internal ACP service backed by its SDK client. [[ACP agent adapter](https://github.com/anomalyco/opencode/blob/7f964bbb00e505178847e2c08721b0fff56208f9/packages/opencode/src/acp/agent.ts)] **Fact**

### 7.2 Patterns worth adopting

**Recommendation:** copy these ideas:

- headless core/server independently usable from the primary UI;
- one authoritative typed contract and generated clients;
- events as a first-class stream, not polling UI state;
- embedded and network modes with equivalent semantics;
- desktop sidecar ownership and health checks;
- ACP as an adapter rather than the core model;
- tests that prevent core/server dependencies from leaking into clients.

### 7.3 Patterns to evaluate, not copy blindly

**Inference:** routing an in-process SDK through the full HTTP router maximizes semantic parity but may retain serialization/routing concepts inside trusted embedding. For this harness, a typed engine interface should be authoritative; protocol adapters should be tested for behavioral equivalence. An embedded adapter may call the engine directly rather than simulate HTTP if parity tests are strong.

OpenCode's large package graph reflects its history, runtime, and product scope. It is evidence that multi-client separation works, not evidence that a new repository should begin with dozens of packages.

## 8. Design-It-Twice — Option A: minimum engine interface

This option deliberately minimizes the engine interface to three entry points. It maximizes depth and places variation behind request and event types.

### 8.1 Interface

Illustrative Rust—not an implementation commitment:

```rust
pub async fn open(
    config: EngineConfig,
    adapters: EngineAdapters,
) -> Result<Engine, OpenError>;

impl Engine {
    pub async fn request(
        &self,
        request: Request,
    ) -> Result<Response, RequestError>;

    pub async fn subscribe(
        &self,
        subscription: Subscription,
    ) -> Result<EventStream, SubscribeError>;
}
```

Shutdown is a `Request::Shutdown { deadline }`; dropping the final handle is a best-effort fallback, not the graceful-shutdown contract.

Representative request families:

```rust
pub enum Request {
    StartRun(StartRun),
    DecideApproval(DecideApproval),
    CancelRun(CancelRun),
    GetRun(GetRun),
    ListSessions(ListSessions),
    GetArtifact(GetArtifact),
    Shutdown(Shutdown),
}
```

The public process protocol does not have to expose one literal `request` method; its typed HTTP routes or JSON-RPC methods may map to this internal sum type.

### 8.2 Interface invariants

The full interface includes these promises:

1. **Ownership:** `open` creates one explicitly owned engine lifecycle. It validates configuration and adapter compatibility before returning.
2. **Admission:** a mutating request contains a scope and idempotency key. A successful `Accepted` response identifies the first durable event/cursor that represents admission. No success is returned for work that can disappear without a corresponding recovery record when durability is configured.
3. **Ordering:** events are monotonic within a run/aggregate. There is no promised total order across unrelated runs.
4. **Visibility:** once a response exposes an event cursor, a subscription opened after that cursor can observe the committed event unless retention policy has explicitly expired it.
5. **At-least-once replay:** reconnect/replay may redeliver an event; stable event IDs make deduplication possible. The engine does not promise exactly-once network delivery.
6. **Backpressure:** every queue and stream is bounded. A slow subscriber receives coalesced non-semantic text deltas or a typed lag error with a resumable cursor; semantic state events are never silently discarded.
7. **Cancellation:** cancellation is idempotent. Success means the cancellation intent was accepted, not that remote provider or child process work has already stopped. A later terminal event reports the outcome.
8. **Authorization context:** the driving adapter authenticates the caller and passes an already validated `Principal`/scope. The engine enforces resource and capability authorization; it does not trust client-selected scope.
9. **Shutdown:** after shutdown admission, new mutating work is rejected; owned work is drained or cancelled by deadline; a report states incomplete resources.
10. **No transport types:** no HTTP request, JSON value, SQL row, provider event, ACP type, or UI model crosses the engine seam.

### 8.3 Ordering and lifecycle example

```rust
let engine = open(config, adapters).await?;

let started = engine
    .request(Request::StartRun(StartRun {
        principal,
        idempotency_key,
        workspace,
        input,
    }))
    .await?
    .into_started()?;

let mut events = engine
    .subscribe(Subscription::run(started.run_id).after(started.cursor))
    .await?;

while let Some(event) = events.next().await {
    render(event?);
}
```

The client does not construct provider requests, start tools, persist messages, or decide retry ordering. That complexity remains local to the engine implementation.

### 8.4 Error model

```rust
pub enum RequestError {
    Rejected(Rejection),       // invalid input, unsupported capability, limit
    Unauthorized,              // safe public category, no secret detail
    Conflict(Conflict),        // invalid current state/revision
    NotFound(ResourceKind),
    Unavailable(RetryAdvice),  // bounded/retryable admission failure
    Internal(DiagnosticId),    // opaque to caller; details stay in protected logs
}

pub enum SubscribeError {
    InvalidCursor,
    CursorExpired { earliest: EventCursor },
    Unauthorized,
    Unavailable(RetryAdvice),
    Internal(DiagnosticId),
}
```

Transport adapters additionally own parse, framing, protocol-version, method-not-found, and connection errors. Provider error bodies and SQL errors never escape directly.

### 8.5 What the implementation hides

Behind these three entry points sit:

- actor/task ownership and Tokio scheduling;
- bounded queues and admission permits;
- provider capability selection and streaming normalization;
- event journal transactions, snapshots, and projection updates;
- context budgeting, retrieval, summarization, and prompt assembly;
- approval state and tool execution;
- cancellation trees and child-process supervision;
- retries, timeouts, and circuit behavior;
- artifact storage;
- subscriber fan-out, coalescing, replay, and lag handling;
- traces, metrics, and protected diagnostics.

The deletion test is favorable: removing this module would force every client and adapter to reconstruct substantial orchestration and invariants.

### 8.6 Dependency strategy and adapters

- **In-process:** reducers, state machines, budgeting, and event derivation stay private and are tested directly where useful.
- **Local-substitutable:** SQLite and filesystem adapters are exercised with temporary real stores; their seams remain internal until a second backend is real.
- **True external:** providers and MCP/tool processes implement engine-owned ports; deterministic fakes drive tests.
- **Driving adapters:** stdio JSON-RPC, HTTP/SSE, ACP, and in-process embedding all translate to the same request and subscription semantics.
- **Composition:** `aranyd` constructs the adapters and calls `open`; the engine never discovers concrete implementations globally.

### 8.7 Trade-offs

**High leverage:** three entry points cover all use cases and make behavioral tests stable across internal refactors. Ordering, durability, cancellation, and error policy have one locality.

**Cost:** complexity can migrate into large request/response enums. Adding a Rust enum variant is a source-level breaking change for exhaustive downstream matches, so the interface should remain internal to workspace adapters or use `#[non_exhaustive]` where external Rust consumption is intended. The wire protocol must independently define unknown method/event behavior.

**Thin area:** query-heavy clients may prefer generated typed methods rather than constructing a `Request` enum. Generated clients can provide that ergonomic facade without widening the engine interface.

**Risk:** a single `request` method can become an unstructured message bus. Prevent this with typed request variants, per-variant validation, explicit response types, ownership documentation, and no arbitrary string dispatch inside the engine.

**Option-A verdict:** strongest initial engine seam. Keep protocol-specific ergonomic surfaces outside it.

### 8.8 Recommended hybrid: deep engine below, caller-first SDK above

Minimizing the engine interface does not require every product caller to construct `Request` enums. **Recommendation:** keep Option A as the semantic seam, then provide a caller-first facade in each supported client language:

```rust
let run = client.start_run(input).await?;

while let Some(update) = run.updates().await? {
    render(update);
}
```

`RunHandle` is a convenience object containing stable identifiers plus a client reference. It does not own engine state and it must be safe to reconstruct after reconnect:

```rust
pub struct RunHandle {
    client: HarnessClient,
    run_id: RunId,
}

impl RunHandle {
    pub async fn cancel(&self) -> Result<Receipt, ClientError>;
    pub async fn snapshot(&self) -> Result<RunView, ClientError>;
    pub async fn updates(&self, after: Option<EventCursor>)
        -> Result<ClientEventStream, ClientError>;
}
```

Continuing a conversation starts a new run in the existing session:

```rust
let next = client.continue_session(run.session_id(), input).await?;
```

A run accepts its initial input once and then progresses to exactly one terminal outcome. `RunHandle` observes or cancels that execution; it is not a mutable conversation object.

The facade maps to generated HTTP/OpenAPI methods, JSON-RPC, or an embedded Rust adapter. Its methods may vary idiomatically by language, while their semantics come from the single process contract and engine interface. This preserves both kinds of leverage:

- the engine has three stable entry points and one locality for invariants;
- the common caller gets a discoverable, typed workflow;
- a C++ or TypeScript SDK can implement the same facade without linking Rust;
- transport, reconnection, idempotency, and cursor handling are hidden in the client implementation;
- advanced callers can use the lower-level generated client without widening the engine.

Do not serialize a `RunHandle` or treat it as exclusive ownership. Multiple clients may observe the same durable run, so authority comes from the authenticated principal and capability policy, not possession of an object reference.

## 9. Clean implementation rules for this architecture

### 9.1 Names and vocabulary

Use domain names that expose responsibility: `RunJournal`, `ContextCompiler`, `ApprovalDecision`, `ToolInvocation`, `ProviderExchange`. Avoid generic `Manager`, `Helper`, `Utils`, and `Service` unless the domain genuinely uses the term.

One concept gets one authoritative name across engine, protocol translation, tests, and documentation. Adapter-specific names may differ at the edge and are translated once.

### 9.2 Functions and side effects

Keep pure decision logic separate from effect execution where this makes the policy obvious:

```text
previous state + command -> decision/events
events + adapters        -> effects
previous state + event   -> next state
```

This does not require every function to be tiny. Split a function when the extracted part has a cohesive name and hides information, not merely to satisfy a line count. Ousterhout explicitly notes that shorter is generally better but decomposition is useful only when it can be done cleanly. [[Ousterhout, Modular Design](https://web.stanford.edu/~ouster/cgi-bin/cs190-winter18/lecture.php?topic=modularDesign)] **Fact**

### 9.3 Errors

Use typed errors at module interfaces. Include facts the caller can act upon: conflict revision, retry delay, expired cursor, unsupported capability. Hide provider payloads, SQL statements, filesystem layout, secrets, and internal stack details.

Define impossible states out of the public interface where economical, but do not create dozens of wrapper types that callers constantly unwrap. Depth matters more than type-count purity.

### 9.4 Comments and documentation

Comments explain invariants, ordering, non-obvious safety, or why a compromise exists. They do not narrate the code. Interface documentation must include failure and ordering behavior because those are part of the interface.

Create `agents/<module>.md` only after a real module exists. It should contain:

- responsibility and hidden decisions;
- public interface and invariants;
- allowed and forbidden dependencies;
- event/ordering/durability rules;
- relevant validation commands;
- platform-specific failure modes.

Architecture decisions with alternatives and trade-offs belong in ADRs, not module agent rules. Generated configuration and commands remain discoverable from repository files rather than duplicated into prose.

## 10. Security consequences of the seams

Modularity is not automatically isolation. The process protocol, provider adapters, MCP, tools, repositories, and renderers are trust boundaries.

### 10.1 Assets and attacker-controlled inputs

Assets include provider credentials, source files, canonical run history, durable memory, artifacts, tool authority, user approvals, and billing/cost budget. Attacker-controlled inputs include prompts, repository text, retrieved memory, provider output, MCP responses, tool output, protocol payloads, filenames, URLs, and rendered Markdown/HTML.

### 10.2 Required controls

**Recommendation:**

- validate typed payload shape, size, depth, count, and nesting before allocation-heavy work;
- authenticate remote clients and authorize every resource and capability in the engine, not only the UI;
- bind network adapters to loopback by default; remote mode requires TLS, authentication, origin policy, and session policy;
- protect local Unix sockets/named pipes with OS permissions; use a random bearer capability when TCP loopback is used;
- never allow a client-supplied workspace/session identifier to bypass the authenticated scope;
- keep secrets and raw prompt/tool/memory contents out of events, errors, and telemetry by default;
- treat provider, repository, memory, MCP, and tool text as data, never policy;
- invoke tools with executable plus argument array, not shell interpolation;
- bound concurrency, queues, stdout/stderr, artifacts, request bodies, decompression, and retries;
- supervise and reap complete child-process trees;
- sanitize untrusted Markdown, HTML, SVG, links, and file names at each render adapter;
- capability-negotiate optional client actions such as terminal and filesystem access;
- keep authorization decisions server-side even when a client presents the approval UI.

ACP's schema itself models capability negotiation and permission requests, but adopting ACP does not remove the need to authenticate the client or authorize the underlying resource. **Inference**

### 10.3 FFI-specific risk

If a future C ABI is approved, every pointer, length, callback, allocator boundary, thread-affinity rule, and unwind path is part of the security and safety interface. Require explicit invariants, fuzzing at the byte boundary, sanitizers on the C/C++ side, and a prohibition on unwinding across FFI. This is another reason to prefer a process seam initially.

## 11. Verification strategy

### 11.1 Architecture tests

- `arany-domain` has no async runtime, HTTP, SQL, MCP, ACP, provider, or UI dependency.
- `arany-engine` does not depend on concrete adapters or wire types.
- clients do not link engine implementation.
- provider SDK types occur only inside provider adapters.
- protocol schema generation is reproducible and checked for drift.
- every public adapter passes the same behavioral conformance suite.

### 11.2 Behavior tests

- Pure reducer/property tests for legal and illegal transitions.
- Engine interface tests with deterministic provider/tool/clock/id adapters.
- Success plus meaningful failure for every use case.
- Idempotent replay of duplicate mutating requests.
- Crash between event commit and projection/index work.
- Reconnect from every emitted cursor and deduplicate repeated events.
- Cancellation during provider streaming, approval wait, tool execution, storage commit, and shutdown.
- Bounded overload: admission rejection, no unbounded RSS growth, no silent semantic event loss.
- Provider contract tests from recorded sanitized fixtures plus selected live smoke tests.
- ACP conformance tests and generated-client tests.
- C++/web smoke clients built only from the published schema, proving no Rust implementation dependency.

### 11.3 Compatibility tests

Maintain golden protocol fixtures for at least current and previous supported versions. Test old client/new daemon and new client/old daemon for every advertised compatible pair. Unknown optional fields and events must have documented behavior. Removed Protobuf field numbers, if Protobuf is selected, remain reserved as required by its evolution guide. [[Protocol Buffers proto3 guide](https://protobuf.dev/programming-guides/proto3/#deleting-fields)] **Fact**

### 11.4 Performance tests

Measure the seam rather than assuming it:

- typed in-process request admission;
- stdio JSON-RPC and HTTP request admission;
- delta relay under realistic chunk sizes;
- CPU/RSS under many idle and active sessions;
- latency added by event persistence and replay;
- terminal render throughput for Rust and any C++ prototype.

**Gate:** adopt C++ for the CLI only if it provides a measured benefit in terminal UX, ecosystem, binary constraints, or contributor ownership that exceeds the permanent cost of a second toolchain. It should not be selected merely because C++ is native.

## 12. Phased implementation plan

### Phase 0 — prove the seams, not the feature set

Build:

- minimal domain vocabulary and run reducer;
- Design-It-Twice comparison of engine interfaces, including Option A above;
- deterministic fake streaming provider;
- in-memory journal;
- one engine request and resumable event stream;
- stdio JSON-RPC and HTTP/SSE adapters over the same semantics;
- tiny Rust CLI using the process protocol;
- tiny TypeScript or browser client generated from the schema;
- ACP mapping spike for initialize/new session/prompt/update/cancel.

Exit gates:

- engine tests do not import transport or provider types;
- both transports pass the same conformance cases;
- reconnect and cancellation semantics are explicit;
- measured owned overhead is within the latency budget;
- the interface remains comprehensible without reading the implementation.

### Phase 1 — durable local harness

Add:

- SQLite journal and snapshots;
- artifact store;
- one real provider plus fake provider;
- context compiler with explicit budgets;
- supervised native tool execution and approvals;
- daemon discovery/ownership and graceful shutdown;
- protocol version/capability negotiation;
- structured tracing and protected diagnostic IDs.

Exit gates:

- forced-kill recovery preserves acknowledged mutations;
- idempotent resend has no duplicate logical effect;
- all buffers, output, concurrency, and retries have tested bounds;
- loopback/local authorization is enforced;
- storage and provider changes do not touch clients or domain.

### Phase 2 — multiple real clients and extensions

Add only after Phase 1 is stable:

- web client over HTTP/SSE;
- desktop sidecar wrapper if needed;
- full ACP adapter;
- MCP tool/resource adapter;
- second provider to prove the provider seam;
- second UI implementation or C++ CLI spike to prove the client seam.

Exit gate: at least two driving adapters and two meaningful driven adapter families work without special-case logic in the engine.

### Phase 3 — split crates where pressure is real

Use measured build times, dependency graphs, platform packaging, independent releases, and ownership to decide which `arany-adapters` modules become crates. Consider WASI/process plugin seams only after the internal capability and lifecycle model is stable. Do not expose Rust dynamic-library plugins: the Rust ABI has no stability guarantee. **Recommendation based on cited Rust ABI fact**

## 13. Decisions to record before implementation

Create ADRs for these choices after their spikes:

1. Authoritative engine interface selected from Design-It-Twice.
2. Canonical process contract and schema technology.
3. Event acknowledgement, replay, and retention semantics.
4. Daemon lifecycle: per-client child, shared local daemon, or both.
5. Local authentication and socket/port discovery.
6. Workspace and tenant scope model.
7. Storage transaction boundary and source of truth.
8. Provider capability model and raw-extension policy.
9. Tool approval and sandbox capability model.
10. Conditions that justify a new crate, process, or FFI surface.

Each ADR should state context, decision, reason, alternatives, trade-offs, scope, and status. Do not turn research recommendations into immutable rules without the corresponding spike evidence.

## 14. Anti-goals

- No crate-per-noun or trait-per-dependency architecture.
- No UI state, provider JSON, SQL row, or MCP/ACP type in domain policy.
- No “universal plugin” abstraction before capabilities, lifecycle, cancellation, and trust are defined.
- No Rust `dylib` seam for third-party polyglot clients.
- No direct database access from clients.
- No client-specific engine branches such as `if client == "desktop"`.
- No requirement that every frontend implement all capabilities; negotiate them.
- No global singleton engine or hidden adapter discovery.
- No unbounded event broadcaster or tool output.
- No exact-once network-delivery claim; use idempotency and replay.
- No copying OpenCode's package graph without reproducing the reason for each seam.

## 15. Final recommendation

The architecture should be **Rust-first, not Rust-everywhere**. Rust owns the durable agent semantics, concurrency ownership, resource bounds, and external capability orchestration. Clients own presentation and may be implemented in Rust, C++, TypeScript, or another language because the real public seam is a versioned protocol, not a Rust crate.

Use Clean Architecture for dependency direction, Parnas for decomposition, Ports and Adapters for external seams, Clean Code for implementation hygiene, and deep-module thinking for interface quality. These ideas reinforce one another when applied at their proper scale:

- dependencies point toward policy;
- modules hide likely-to-change decisions;
- adapters translate technologies at real seams;
- code inside a module stays clear and tested;
- the engine interface remains much smaller than the behavior it provides.

Begin with four meaningful Rust crates and two binaries, not the final imagined ecosystem. Prove the process protocol with a second-language client early. Add crate and process boundaries only when they improve dependency control, release cadence, ownership, compile locality, test isolation, or fault isolation. This gives future contributors freedom without charging the first implementation for hypothetical flexibility.

## Primary-source bibliography

### Architecture and modularity

- Robert C. Martin, [“The Clean Architecture”](https://blog.cleancoder.com/uncle-bob/2012/08/13/the-clean-architecture.html), 2012.
- Robert C. Martin, [“The Single Responsibility Principle”](https://blog.cleancoder.com/uncle-bob/2014/05/08/SingleReponsibilityPrinciple.html), 2014.
- Robert C. Martin / Clean Coders, [Clean Code course outline](https://cleancoder.com/files/cleanCodeCourse.md).
- Robert C. Martin, [*Design Principles and Design Patterns*](https://objectmentor.com/resources/articles/Principles_and_Patterns.pdf), including package cohesion/coupling principles.
- David L. Parnas, [“On the Criteria To Be Used in Decomposing Systems into Modules”](https://dl.acm.org/doi/10.1145/361598.361623), *Communications of the ACM* 15(12), 1972.
- Alistair Cockburn, [original Hexagonal Architecture / Ports and Adapters article](https://alistair.cockburn.us/hexagonal-architecture/), 2005.
- Alistair Cockburn, [*Component + Strategy generalizes Ports & Adapters*](https://alistaircockburn.com/Component%20plus%20strategy.pdf), 2023 revision.
- John Ousterhout, [Modular Design lecture notes](https://web.stanford.edu/~ouster/cgi-bin/cs190-winter18/lecture.php?topic=modularDesign), Stanford CS 190.
- John Ousterhout, [The Nature of Complexity lecture notes](https://web.stanford.edu/~ouster/cgi-bin/cs190-winter18/lecture.php?topic=complexity), Stanford CS 190.

### Rust and cross-language interfaces

- Rust Project, [Packages and Crates](https://doc.rust-lang.org/book/ch07-01-packages-and-crates.html), *The Rust Programming Language*.
- Rust Project, [Cargo Workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html), *The Cargo Book*.
- Rust Project, [Visibility and Privacy](https://doc.rust-lang.org/reference/visibility-and-privacy.html), *The Rust Reference*.
- Rust Project, [Linkage](https://doc.rust-lang.org/reference/linkage.html), *The Rust Reference*.
- Rust Project, [External blocks and ABIs](https://doc.rust-lang.org/reference/items/external-blocks.html#abi), *The Rust Reference*.
- Rust Project, [SemVer Compatibility](https://doc.rust-lang.org/cargo/reference/semver.html), *The Cargo Book*.

### Protocols

- JSON-RPC Working Group, [JSON-RPC 2.0 Specification](https://www.jsonrpc.org/specification).
- OpenAPI Initiative, [OpenAPI Specification](https://spec.openapis.org/oas/).
- Protocol Buffers project, [Proto3 Language Guide: updating message types](https://protobuf.dev/programming-guides/proto3/#updating).
- Agent Client Protocol project, [official repository and versioning notes](https://github.com/agentclientprotocol/agent-client-protocol).
- Agent Client Protocol project, [ACP v1 JSON Schema](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/schema/v1/schema.json).
- Model Context Protocol project, [official Go SDK protocol and transport documentation](https://go.sdk.modelcontextprotocol.io/protocol/).
- Microsoft, [official Language Server Protocol overview](https://microsoft.github.io/language-server-protocol/).

### OpenCode

- OpenCode, [Server documentation](https://opencode.ai/docs/server/).
- OpenCode, [SDK documentation](https://opencode.ai/docs/sdk/).
- OpenCode, [embedded SDK documentation](https://opencode.ai/v2/docs/build/sdk).
- OpenCode source at audited commit [`7f964bbb00e505178847e2c08721b0fff56208f9`](https://github.com/anomalyco/opencode/tree/7f964bbb00e505178847e2c08721b0fff56208f9), especially `packages/protocol`, `packages/server`, `packages/client`, `packages/sdk-next`, `packages/tui`, `packages/app`, `packages/desktop`, and the ACP adapter.
