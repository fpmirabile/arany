# Provider adapters and the tool/effect runtime

**Status:** research and architectural recommendation  
**Research date:** 2026-09-28  
**Question:** How should a Rust-first harness integrate fast-changing model providers and execute native, MCP, interactive, and background tools without losing provider features, durability, security, or user-visible feedback?

> **Scope amendment — 2026-09-29:** Version 1 implements only deterministic fake and bounded non-streaming OpenAI behind the Provider seam. It has no effectful Tool, effect coordinator, MCP, PTY, background process, or provider streaming. Those designs below activate on their named triggers; the [system overview](../architecture/system-overview.md) and [next-step decision register](./next-step-decision-register.md) are canonical.

## Reading guide

This report uses four labels deliberately:

- **Fact**: directly supported by a cited primary source: official documentation, a specification, an official source repository, or an original engineering reference.
- **Inference**: a conclusion derived from those facts, but not itself guaranteed by a source.
- **Recommendation**: the proposed design for this repository.
- **Gate**: a claim that must be proved by a spike, conformance test, or measurement before it becomes a lasting decision.

The word **effect** means an externally observable operation such as starting a process, writing a file, calling an MCP tool, or sending a remote mutation. Model generation is also external I/O, but it uses a provider-specific reliability path because partial output, billing, and provider-side state have different semantics.

## Executive conclusion

Rust is feasible for both the provider hot path and the tool runtime, but speed does not come from pretending all providers and all tools are identical. The core should normalize **lifecycle semantics**, not erase provider capabilities or execution mechanisms.

**Recommendation:** establish two deep boundaries:

1. A **provider driver** accepts a portable semantic request plus explicit required/optional capabilities, compiles it through a provider adapter, and emits one ordered internal event stream. Provider-specific typed extensions remain available, so the abstraction does not collapse to a lowest common denominator. The driver owns admission control, deadlines, retry and resumption policy, cancellation state, credential leasing, and conformance evidence.
2. An **effect coordinator** turns a proposed tool call into a canonical, policy-bound `EffectIntent`, durably records it before dispatch, obtains any required approval, leases it to a tool backend, supervises execution, bounds all output, and records either a confirmed outcome or an explicit `Uncertain` state. Native processes, MCP, later WASI components, interactive PTYs, and background jobs share this lifecycle without sharing one transport implementation.

```text
model request                           proposed tool call
      │                                         │
      ▼                                         ▼
provider driver                         effect coordinator
  capability check                     validate + canonicalize
  admission/retry                      policy + approval
  stream normalization                 durable intent/outbox
      │                                         │
      ▼                                         ▼
provider adapter                       tool backend adapter
OpenAI / Anthropic / Gemini         process / MCP / later WASI
      │                                         │
      ▼                                         ▼
typed ProviderEvent stream       bounded events + artifact refs
```

The provider interface must preserve text, structured data, tool calls, provider-authored reasoning summaries, opaque reasoning items, token usage, request identifiers, resumability, and unknown extensions without leaking provider wire structs into the domain. The tool interface must preserve authority, approval, idempotency, process-tree ownership, workspace identity, output bounds, and recovery state without treating an MCP server or a process name as trusted.

This extends, rather than replaces, the repository's existing decisions:

- It keeps `clients -> protocol adapters -> engine -> domain` and treats providers and tools as driven adapters, as proposed in [the modular architecture report](./modular-harness-architecture.md).
- It keeps authorization semantics in the domain/engine and enforcement in the protection runtime, as proposed in [the deterministic protection report](./deterministic-harness-protection.md).
- It emits durable semantic milestones and coalescible high-frequency deltas for the event reducer described in [the multi-agent loop report](./multi-agent-loop-and-user-feedback.md).
- It accepts provider integration velocity as the main Rust risk identified in [the Rust feasibility report](./rust-core-feasibility.md), but mitigates it with generated wire types where available, captured fixtures, conformance probes, and an optional official-SDK sidecar rather than moving the engine out of Rust.

The strongest design rule is: **a provider adapter interprets a provider protocol; a tool backend performs an already-authorized effect; neither decides agent policy.**

## 1. Scope and invariants

### 1.1 What belongs in the Engine

**Recommendation:** the engine owns these cross-provider and cross-tool invariants:

- one run-level deadline and cancellation lineage;
- declared limits for concurrency, queues, stream buffers, tool output, artifacts, retries, and stored envelopes;
- capability requirements checked before a request is sent;
- deterministic effect identity and canonical arguments;
- policy and approval proofs bound to the exact effect;
- durable lifecycle events and recovery decisions;
- provider and tool failure classification;
- secret-safe observability;
- a stable event vocabulary for all clients.

The engine does **not** own provider authentication syntax, provider JSON fields, SSE framing differences, MCP JSON-RPC mechanics, PTY syscalls, cgroup or Job Object setup, or frontend rendering. Those are changeable mechanisms hidden by adapters.

### 1.2 Non-negotiable invariants

**Recommendation:** encode these as tests and, where practical, types:

1. No request is sent when a required capability is unsupported or only approximately emulated.
2. No provider wire object enters domain state or the client protocol.
3. No effect is dispatched before its canonical intent and authorization decision are durable.
4. No approval can authorize different arguments, tool version, workspace, authority, or expiry from those displayed.
5. No automatic retry may duplicate an effect unless the downstream operation supplies an idempotency or reconciliation contract that the coordinator actually uses.
6. No child process receives ambient environment variables, inherited handles, workspace access, or network authority merely because the harness process has them.
7. Every queue, stream, output, artifact, retry loop, job, and retained envelope has a configured bound and declared overflow behavior.
8. Cancellation has a recorded outcome; a closed client connection is not treated as proof that a provider or process stopped.
9. Secrets and raw user/model/tool content are absent from logs and traces by default.
10. Recovery never silently converts an unknown external outcome into success, failure, or a replay.

## 2. Provider normalization without capability loss

### 2.1 Normalize semantics, not every wire field

Provider APIs overlap, but their semantics are not identical. OpenAI's Responses API exposes typed response items, function calls, structured output, background responses, and reasoning items. [[OpenAI Responses migration guide](https://developers.openai.com/api/docs/guides/migrate-to-responses)] [[OpenAI reasoning guide](https://developers.openai.com/api/docs/guides/reasoning)] **Fact** Anthropic streams content blocks whose deltas may contain text, partial tool-input JSON, thinking, signatures, and usage; its documentation explicitly tells clients to tolerate unknown event types. [[Anthropic streaming Messages](https://platform.claude.com/docs/en/build-with-claude/streaming)] **Fact** Gemini has native function calling, structured outputs, streaming, and Live API session-resumption semantics that do not map exactly onto either API. [[Gemini function calling](https://ai.google.dev/gemini-api/docs/function-calling)] [[Gemini structured output](https://ai.google.dev/gemini-api/docs/structured-output)] [[Gemini text generation](https://ai.google.dev/gemini-api/docs/text-generation)] **Fact**

**Inference:** one universal request/response JSON document will either leak vendor concepts everywhere or discard valuable features. OpenAI-compatible HTTP is useful as one adapter protocol, but Google explicitly notes that its OpenAI compatibility layer does not expose every Gemini-specific feature. [[Gemini OpenAI compatibility and direct integration](https://ai.google.dev/gemini-api/docs/partner-integration)] **Fact**

**Recommendation:** use a layered request:

```rust
struct ModelRequest {
    conversation: Vec<InputItem>,
    tools: Vec<ToolDefinition>,
    output: OutputContract,
    requirements: CapabilityRequirements,
    limits: GenerationLimits,
    provider_options: ProviderOptions,
}

enum ProviderOptions {
    None,
    OpenAi(OpenAiOptions),
    Anthropic(AnthropicOptions),
    Gemini(GeminiOptions),
    Extension(ValidatedProviderExtension),
}
```

The portable layer represents concepts the engine must reason about: input parts, tool definitions, requested output contract, budgets, continuation identity, and required versus optional capabilities. The typed provider layer preserves deliberate use of provider-only features. A final extension escape hatch may carry adapter-scoped canonical JSON, but it must be versioned, schema-checked, size-bounded, and forbidden from changing base URLs, credentials, authorization headers, transport security, or policy without a separate grant.

This is a **progressive-capability abstraction**: callers can remain portable, opt into a named provider, or explicitly fail when a required feature is unavailable. It is not a promise that a request can move between providers unchanged.

### 2.2 Capability negotiation

MCP performs a real initialization handshake in which client and server exchange versions and capabilities, and the specification says participants must not use features that were not negotiated. [[MCP lifecycle specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)] **Fact** Model providers generally do not offer one equivalent handshake covering all model, account, region, and API-version behavior. Their capabilities may depend on the exact model snapshot, endpoint, credential tier, and deployment.

**Recommendation:** define capability evidence rather than a single boolean table:

```rust
struct ProviderCapabilities {
    provider: ProviderId,
    endpoint_family: EndpointFamily,
    model: ModelIdentity,
    features: BTreeMap<Capability, Support>,
    limits: ProviderLimits,
    reliability: ReliabilitySemantics,
    evidence: CapabilityEvidence,
}

enum Support {
    Native { constraints: CapabilityConstraints },
    Emulated { constraints: CapabilityConstraints, caveat: String },
    Unsupported,
    Unknown,
}

struct CapabilityEvidence {
    source: EvidenceSource,
    observed_at: Timestamp,
    adapter_version: AdapterVersion,
    provider_api_version: Option<String>,
}
```

Populate it from, in decreasing order of confidence:

1. a pinned adapter manifest tied to official documentation and a specific API/model version;
2. provider metadata or negotiation when the provider supplies it;
3. a bounded live probe that exercises the actual credential, endpoint, and model;
4. cached evidence with an expiry and explicit staleness.

`required` capabilities fail closed on `Emulated`, `Unsupported`, `Unknown`, or stale evidence unless the request explicitly permits a named emulation. `optional` capabilities produce a recorded degradation decision visible to the orchestrator and UI.

Capabilities must carry constraints, not just flags. Examples include the accepted JSON Schema subset, maximum schema size, whether parallel tool calls are supported, whether tool calls can be forced, whether usage is incremental or final only, whether a response is resumable, and whether cancellation has provider acknowledgment.

### 2.3 One ordered provider event vocabulary

Streaming is part of the contract, not merely an optimization. OpenAI streams server-sent response events and warns that handling partial content complicates moderation. [[OpenAI streaming Responses](https://developers.openai.com/api/docs/guides/streaming-responses)] **Fact** Anthropic may return an HTTP 200 before a streaming error occurs and documents an event grammar with per-content-block deltas. [[Anthropic streaming Messages](https://platform.claude.com/docs/en/build-with-claude/streaming)] [[Anthropic API errors](https://platform.claude.com/docs/en/api/errors)] **Fact**

**Recommendation:** adapters parse wire bytes incrementally into ordered events carrying both a run sequence and provider provenance:

```rust
enum ProviderEvent {
    ResponseStarted(ResponseStarted),
    TextDelta(TextDelta),
    StructuredOutputDelta(StructuredOutputDelta),
    ReasoningSummaryDelta(ReasoningSummaryDelta),
    OpaqueReasoningItem(OpaqueReasoningItem),
    ToolCallStarted(ToolCallStarted),
    ToolArgumentsDelta(ToolArgumentsDelta),
    ToolCallReady(ToolCallReady),
    UsageUpdated(Usage),
    ResponseCompleted(ResponseCompleted),
    ResponseFailed(ResponseFailure),
    ProviderExtension(ProviderExtensionEvent),
}
```

The adapter validates the provider event grammar and emits a terminal event exactly once. It must correctly handle arbitrary byte and SSE chunk boundaries, JSON values split across chunks, UTF-8 code points split across buffers, duplicate terminal notifications, a network error after partial output, and newly introduced event kinds.

Known events become typed events. Unknown provider events are never silently treated as text or success: preserve a bounded typed extension or a reference to an opt-in encrypted raw envelope, then continue only if the event is declared ignorable. Otherwise fail with `UnsupportedProviderEvent`. This follows Anthropic's forward-compatibility requirement without letting unknown data mutate the engine.

The provider stream feeds two consumers:

- the durable run reducer receives semantic milestones such as response start, completed tool call, final usage, completion, failure, and cancellation outcome;
- the feedback plane receives bounded, coalescible text/reasoning/argument deltas with sequence and gap markers.

That split preserves the event-sourced agent loop without forcing a database write per token.

### 2.4 Tool calls and structured output

OpenAI function calls stream argument deltas and require the application to execute the function and return an output. [[OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling)] **Fact** Anthropic represents client tool invocation as `tool_use` content and expects a corresponding `tool_result`. [[Anthropic tool-use overview](https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview)] **Fact** Gemini also distinguishes model function-call proposals from application execution. [[Gemini function calling](https://ai.google.dev/gemini-api/docs/function-calling)] **Fact**

**Recommendation:** provider adapters produce an untrusted `ToolCallReady` only after the complete argument payload passes syntax and declared-size checks. The engine then performs independent tool lookup, JSON Schema validation, normalization, policy evaluation, and effect construction. A provider's tool-call identifier is correlation data, never the authoritative `EffectId`.

Structured output is a separate contract from tool calling. OpenAI recommends function calling when a model connects to application functionality and structured text output when the model is producing a user-facing structured response; strict schemas are constrained to a supported subset. [[OpenAI structured outputs](https://developers.openai.com/api/docs/guides/structured-outputs)] **Fact** Gemini likewise supports only a subset of JSON Schema for structured outputs. [[Gemini structured output](https://ai.google.dev/gemini-api/docs/structured-output)] **Fact**

**Recommendation:** `OutputContract::StrictJsonSchema` is accepted only when capability evidence covers the exact schema features used. The adapter must reject unsupported keywords before sending the request. It must not quietly replace strict structured output with “please return JSON” prompting. `OutputContract::BestEffortJson` may opt into that weaker behavior, and the final result must report that it was emulated.

### 2.5 Reasoning is not ordinary text

OpenAI says reasoning tokens may be billed and counted even though raw reasoning is not exposed; applications can request a provider-authored reasoning summary, and reasoning items must be preserved across tool turns for relevant models. [[OpenAI reasoning guide](https://developers.openai.com/api/docs/guides/reasoning)] **Fact** Anthropic's extended-thinking stream contains thinking and signature deltas and requires thinking blocks to be passed back unchanged in subsequent requests. [[Anthropic streaming Messages](https://platform.claude.com/docs/en/build-with-claude/streaming)] [[Anthropic API errors](https://platform.claude.com/docs/en/api/errors)] **Fact**

**Recommendation:** distinguish:

- user-visible assistant text;
- a provider-authored reasoning **summary**, explicitly labeled and safe to render under product policy;
- an opaque reasoning item required only for provider continuity;
- usage classified as reasoning tokens;
- internal orchestrator rationale, which is its own harness event and not inferred from provider internals.

Never convert opaque reasoning into text or log it. Store continuity items with the same access restrictions and retention discipline as conversation content, and pass them only back to the originating provider/model family when required.

### 2.6 Usage and cost

OpenAI's token-counting documentation notes that output usage can include invisible formatting and reasoning tokens. [[OpenAI token counting](https://developers.openai.com/api/docs/guides/token-counting)] **Fact** Anthropic streams usage fields that may be cumulative rather than per-delta. [[Anthropic streaming Messages](https://platform.claude.com/docs/en/build-with-claude/streaming)] **Fact**

**Recommendation:** do not force every provider into only `input_tokens` and `output_tokens`:

```rust
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    billed_units: Vec<BilledUnit>,
    kind: UsageKind, // estimate, incremental, cumulative, final
    provider_fields: BoundedMap<String, ScalarValue>,
}
```

Final authoritative provider usage replaces estimates; it is not added to cumulative deltas. Cost is a separate projection using a versioned pricing snapshot, because price is not a stable provider protocol fact. Budgets operate on both conservative estimates before admission and confirmed usage afterward.

## 3. Provider reliability and lifecycle

### 3.1 Admission control before retries

Providers expose multiple quota dimensions. OpenAI documents rate-limit headers and recommends exponential backoff with jitter, while noting that unsuccessful requests still contribute to limits. [[OpenAI rate limits](https://developers.openai.com/api/docs/guides/rate-limits)] **Fact** Anthropic applies token-bucket limits across organization and workspace dimensions and exposes retry/reset headers. [[Anthropic rate limits](https://platform.claude.com/docs/en/api/rate-limits)] **Fact** Gemini limits may apply across requests and token dimensions by project and model. [[Gemini rate limits](https://ai.google.dev/gemini-api/docs/rate-limits)] **Fact**

**Recommendation:** one engine-owned admission controller allocates provider work by at least provider, endpoint, credential/project, model, request count, estimated input tokens, estimated output tokens, concurrent streams, and run priority. It honors provider reset and `Retry-After` information and exposes queue position to the user. A provider SDK or sidecar must not add an unobserved nested retry loop.

### 3.2 Failure taxonomy

Adapters should return classified failures, never a single string:

```rust
enum ProviderFailureClass {
    InvalidRequest,
    Authentication,
    Authorization,
    UnsupportedCapability,
    QuotaExhausted,
    RateLimited,
    ProviderOverloaded,
    TransportBeforeResponse,
    TransportAfterPartialResponse,
    ProtocolViolation,
    ContentRejected,
    DeadlineExceeded,
    Canceled,
    UnknownOutcome,
}
```

Include provider request ID, HTTP status when applicable, retry hint, whether any response event was consumed, provider operation ID, and a redacted bounded diagnostic. Anthropic documents request IDs and distinguishes overload, rate-limit, timeout, and validation errors. [[Anthropic API errors](https://platform.claude.com/docs/en/api/errors)] **Fact**

### 3.3 Retry rules

Google recommends bounded exponential backoff with jitter only for transient classes such as 408, 429, and 5xx errors. [[Gemini API troubleshooting](https://ai.google.dev/gemini-api/docs/troubleshooting)] **Fact** Both OpenAI and Anthropic warn that errors can occur after a stream begins. [[OpenAI rate limits](https://developers.openai.com/api/docs/guides/rate-limits)] [[Anthropic API errors](https://platform.claude.com/docs/en/api/errors)] **Fact**

**Recommendation:** configure maximum attempts **and** a total retry deadline, then apply this matrix:

| Situation | Automatic action |
|---|---|
| Rejected locally before sending | Never retry; fix the request or capability choice |
| Transient transport/provider failure before any response event | Retry within budget; record possible duplicate provider cost |
| Rate limit with server retry/reset hint | Requeue until the run deadline; surface wait state |
| Any non-resumable stream after content or tool-call data | Do not replay automatically; terminate partial attempt or ask the orchestrator for an explicit regeneration |
| Durable provider operation with a supported cursor/response ID | Resume the same operation, never create a replacement implicitly |
| Authentication, authorization, schema, or unsupported capability | Never retry unchanged |
| Unknown outcome after sending a state-changing provider-side operation | Reconcile when the API supports it; otherwise mark uncertain |

Generation is often logically repeatable but is not byte-deterministic, free, or necessarily deduplicated. Therefore `retry` and `regenerate` are different engine commands and different UI events.

### 3.4 Resumption and cancellation

OpenAI background mode supports polling, cancellation, and resuming a stream from a prior event using `starting_after`, subject to endpoint and retention constraints. [[OpenAI background mode](https://developers.openai.com/api/docs/guides/background)] **Fact** Gemini Live exposes session resumption handles and server `GoAway` signals. [[Gemini Live session management](https://ai.google.dev/gemini-api/docs/live-api/session-management)] [[Gemini Live best practices](https://ai.google.dev/gemini-api/docs/live-api/best-practices)] **Fact** MCP Streamable HTTP can use event IDs to resume delivery after disconnection. [[MCP transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)] **Fact**

**Recommendation:** model resumability as a provider capability carrying cursor type, retention, ordering, and replay guarantees. Persist provider operation IDs and cursors only in durable semantic milestones. Do not advertise resume for ordinary streams merely because the client can reconnect.

Cancellation has these observable states:

```text
requested -> transport_aborted
          -> provider_acknowledged
          -> completed_before_cancel
          -> deadline_exceeded
          -> outcome_uncertain
```

Closing a socket proves only local transport closure. If the provider exposes cancellation, send it and record acknowledgment. Otherwise show `transport_aborted` or `outcome_uncertain`, because billing or provider computation may continue.

## 4. Provider velocity: Rust HTTP and SDK sidecars

### 4.1 Current language support

OpenAI's official library list does not include Rust; it lists `async-openai` as a community library that OpenAI does not verify, while direct HTTP remains supported. [[OpenAI libraries](https://developers.openai.com/api/docs/libraries)] **Fact** Anthropic's official SDK list currently includes Python, TypeScript, C#, Go, Java, PHP, and Ruby, but not Rust. [[Anthropic client SDKs](https://platform.claude.com/docs/en/cli-sdks-libraries/overview)] **Fact** Google's official GenAI SDKs currently cover Python, JavaScript/TypeScript, Go, and Java, while its direct API is language-agnostic and explicitly presented as the path for Rust. Google publishes a machine-readable OpenAPI description for direct integration but warns that manual authentication, type handling, and API changes become the caller's responsibility. [[Gemini libraries](https://ai.google.dev/gemini-api/docs/libraries)] [[Gemini direct API integration](https://ai.google.dev/gemini-api/docs/partner-integration)] **Fact**

**Inference:** waiting for first-party Rust SDK parity would make provider velocity worse. Reimplementing every schema by hand would also be wasteful.

### 4.2 Recommended two-track strategy

**Recommendation:** use direct asynchronous Rust HTTP for the stable production path:

- generate private wire types from a pinned official machine schema where the schema is sufficiently accurate;
- hand-write only streaming state machines, authentication, provider-specific conversions, and known schema gaps;
- keep generated types private to the adapter;
- capture sanitized real response streams as regression fixtures;
- compare behavior against official SDKs in conformance tests;
- pin an API version and exact adapter compatibility manifest.

Use an **official-SDK sidecar** only when it measurably closes a feature gap or acts as a conformance oracle. The sidecar implements the same provider port over a small versioned process protocol, receives a narrowly scoped credential lease, declares its SDK and API versions, and is supervised like any privileged adapter. It is not MCP: provider streaming, usage, resumption, credentials, and error semantics deserve a dedicated protocol rather than being disguised as generic tool calls.

The trade-off is explicit:

| Approach | Strengths | Costs |
|---|---|---|
| Direct Rust HTTP | Lowest steady-state overhead; one runtime; strong types and backpressure; simpler deployment | Manual protocol work; feature lag risk; schema drift; no vendor support for community SDK |
| Official-SDK sidecar | Day-zero vendor feature access; behavior oracle; less hand-maintained wire code | Process lifecycle; serialization; cold start; another dependency ecosystem; credential boundary; SDK-owned hidden retries |

Anthropic's TypeScript repository describes generated SDK code and tests against a mock server derived from its API description, supporting the value of using official SDK behavior as a comparison oracle rather than embedding SDK types in the domain. [[Anthropic TypeScript SDK contributing guide](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/CONTRIBUTING.md)] **Fact**

**Gate:** do not add a sidecar preemptively. First measure direct Rust implementation effort, missing-feature lead time, p50/p99 added latency, memory, startup time, crash recovery, and operational footprint. Add one only when a named provider feature cannot meet its delivery target safely through direct Rust. The provider port must make removal possible.

## 5. Credentials, privacy, and redaction

Provider keys, OAuth tokens, MCP tokens, and secrets injected into tools have a different lifecycle from normal configuration. OWASP recommends centralized secret lifecycle management, least privilege, rotation/revocation, and avoiding secret leakage in logs. [[OWASP secrets management](https://cheatsheetseries.owasp.org/cheatsheets/Secrets_Management_Cheat_Sheet.html)] [[OWASP logging](https://cheatsheetseries.owasp.org/cheatsheets/Logging_Cheat_Sheet.html)] **Fact** Rust's `secrecy` crate makes exposure explicit, redacts `Debug`, and intentionally restricts accidental serialization. [[Rust `secrecy` crate](https://docs.rs/secrecy/latest/secrecy/)] **Fact**

**Recommendation:** durable state stores a `CredentialHandle`, never credential bytes. A credential store resolves that handle just in time into a short-lived `SecretLease` scoped to provider, endpoint, run, and expiry. The adapter receives a non-serializable secret wrapper. Sidecars receive the minimum lease over a protected local channel. Child tools receive a secret only when the specific effect grant includes it.

Secrets must not appear in:

- command arguments, URLs, approval displays, effect IDs, idempotency keys, exception strings, or process titles;
- journals, traces, metrics labels, raw fixture captures, tool previews, artifacts, or crash reports;
- generic environment snapshots or provider request dumps.

Use structural redaction before serialization, followed by bounded canary-based tests for known token patterns. Text replacement alone is insufficient because secrets may be encoded, split across stream chunks, or nested under unexpected fields. The safest default is not to capture raw payloads. OpenAI documents retention differences among endpoints and notes that data sent to third-party MCP servers follows those third parties' policies. [[OpenAI data controls](https://developers.openai.com/api/docs/guides/your-data)] **Fact** The UI must distinguish harness retention from provider and external-tool retention.

## 6. Contract fixtures and live conformance

### 6.1 Three complementary test layers

**Recommendation:** every provider adapter ships with:

1. **Offline protocol fixtures.** Sanitized byte-exact streams and error responses exercise chunk boundaries, unknown fields, duplicated events, truncation, invalid UTF-8, malformed JSON, mid-stream errors, usage changes, tool calls, and reasoning items. Fixtures record provider API version and model identity.
2. **Contract tests.** The adapter is checked against a controllable mock or the behavior of the vendor's official SDK. Generated wire types are diffed when a machine schema changes. JSON Schema behavior can draw from the official cross-implementation test suite. [[JSON Schema Test Suite](https://github.com/json-schema-org/JSON-Schema-Test-Suite)] **Fact**
3. **Opt-in live conformance.** A credential- and budget-gated matrix probes exact model snapshots for each claimed capability. It records evidence without storing sensitive content and is never required for an offline contributor's normal test run.

Fixture tests prove the parser against known inputs. Live probes detect reality drift. Neither alone proves compatibility.

### 6.2 Compatibility manifest

**Recommendation:** publish a generated internal manifest for each release:

```text
adapter version
provider API/endpoint version
tested model identifiers
claimed native/emulated capabilities and constraints
fixture corpus revision
last live probe timestamp and result
known deviations
```

An expired or failing probe does not need to disable all use, but the capability evaluator must stop presenting stale claims as verified facts.

## 7. From a model tool call to a deterministic effect

### 7.1 Separate discovery from authority

`ToolDefinition` is information shown to a model: name, description, input schema, output schema, and declared effects. It is not authority. MCP says tool annotations are untrusted unless obtained from a trusted server, and recommends clear UI and human confirmation for tool invocation. [[MCP tools specification](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)] **Fact**

**Recommendation:** resolve a provider-produced call through these stages:

```text
untrusted ProviderToolCall
        │ syntax and size validation
        ▼
ResolvedToolCall(tool id + pinned implementation version)
        │ JSON Schema validation + canonicalization
        ▼
EffectIntent(effect id + exact authorities + workspace + limits)
        │ deterministic policy
        ▼
Deny | AwaitApproval | Authorized
        │ durable outbox lease
        ▼
ToolBackend execution
```

The resolved tool identifier includes its implementation version or content digest. Canonical arguments preserve semantic values and have a stable hash; never hash raw provider JSON whose object ordering or numeric spelling may differ. Derived defaults are materialized before approval so the displayed and executed intents match.

### 7.2 Effect intent and approval binding

```rust
struct EffectIntent {
    effect_id: EffectId,
    run_id: RunId,
    call_id: AgentCallId,
    tool: ToolRevision,
    canonical_input: CanonicalValue,
    requested_capabilities: CapabilitySet,
    workspace: WorkspaceRevision,
    cwd: RelativeWorkspacePath,
    environment_names: BTreeSet<EnvironmentName>,
    limits: EffectLimits,
    idempotency: IdempotencyContract,
}
```

**Recommendation:** an `ApprovalProof` binds a cryptographic digest of the complete intent, the policy snapshot, approving principal, approval scope, issue time, expiry, and nonce. Any change to tool version, arguments, capabilities, workspace revision, working directory, environment names, limits, or expiry invalidates it. Approval text is a deterministic projection of the same intent, not separately assembled prose.

Approvals authorize one bounded intent or a deliberately defined pattern. They never authorize a provider's free-form description, a shell string that will later be reparsed, or “all similar commands” without a typed similarity scope.

### 7.3 Transactional intent and outbox

The transactional-outbox pattern records domain state and the message to be dispatched in one transaction, avoiding a database/external-system dual write; because delivery can repeat, consumers must be idempotent. [[AWS transactional outbox pattern](https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/transactional-outbox.html)] **Fact** Stripe demonstrates the downstream half of this contract: clients provide a stable idempotency key, and reuse is constrained to the same parameters. [[Stripe idempotent requests](https://docs.stripe.com/api/idempotent_requests)] **Fact** Temporal similarly documents that activities may execute more than once and therefore should be idempotent, with heartbeats/checkpoints supporting recovery. [[Temporal tasks and activities](https://docs.temporal.io/tasks)] **Fact**

**Recommendation:** persist the intent, policy decision, approval reference, and initial outbox state atomically with the agent-loop event that requested the effect. Suggested states are:

```text
Proposed -> Denied
         -> AwaitingApproval -> Authorized
         -> Authorized -> Ready -> Leased -> Running
                                      │          ├─ Succeeded
                                      │          ├─ Failed
                                      │          ├─ Canceled
                                      │          └─ Uncertain
                                      └─ lease expired -> reconcile
```

The dispatcher leases `Ready` work using the stable `EffectId`. The backend passes that identity as a downstream idempotency key when supported. Final outcome and semantic run event are again committed atomically. Leases prevent two local workers from intentionally dispatching the same effect, but leases alone do not prevent a duplicate after a crash.

**Inference:** exactly-once execution across a local database and an arbitrary external tool cannot be promised without cooperation from that tool. A crash after the effect occurred but before the success record committed is ambiguous.

**Recommendation:** each backend declares one of:

- `Idempotent`: retrying the same effect identity and arguments is safe;
- `Reconciliable`: `reconcile(effect_id)` can determine or recover the outcome;
- `AtMostOnceAttempt`: never automatically redispatch after a possibly started attempt;
- `Unspecified`: require conservative `Uncertain` handling.

When recovery cannot prove whether a non-idempotent effect happened, transition to `Uncertain`, show the user the evidence, and require reconciliation or a new explicit intent. Never silently replay it.

## 8. Tool backends: MCP, native processes, and plugins

### 8.1 One lifecycle, different mechanisms

**Recommendation:** use one `ToolBackend` lifecycle but retain typed backend descriptors:

```rust
trait ToolBackend {
    fn descriptor(&self) -> ToolBackendDescriptor;
    async fn prepare(&self, intent: &AuthorizedIntent) -> Result<PreparedEffect, PrepareError>;
    async fn execute(
        &self,
        prepared: PreparedEffect,
        sink: BoundedEffectSink,
        cancel: CancellationToken,
    ) -> EffectOutcome;
    async fn reconcile(&self, effect: EffectId) -> ReconciliationResult;
}
```

`prepare` resolves mechanism-specific details without performing the effect. `execute` receives only already-authorized capabilities. A backend may narrow authority but cannot widen it.

### 8.2 MCP is a protocol, not a sandbox

MCP standardizes discovery and invocation, JSON Schema-described tool input/output, progress, cancellation, and transports. [[MCP tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)] [[MCP progress](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/progress)] [[MCP cancellation](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/cancellation)] **Fact** Its standard transports are stdio and Streamable HTTP. In stdio mode the child must reserve stdout for valid JSON-RPC messages and may log to stderr; for HTTP, servers must validate `Origin`, bind locally when local-only, and use authentication. [[MCP transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)] **Fact**

**Recommendation:** the MCP adapter must:

- pin and negotiate a supported protocol version and capability set;
- validate every advertised schema, result, progress token, request ID, and message size;
- treat names, descriptions, annotations, URIs, embedded resources, and returned content as untrusted data;
- keep protocol stdout separate from human stderr in stdio mode;
- rate-limit progress and notifications;
- map cancellation to MCP cancellation but retain an uncertain state if the server does not acknowledge or terminate;
- enforce authentication, TLS, `Origin`, redirect, DNS, and endpoint policy for remote HTTP;
- supervise a local server as an untrusted process with explicit executable, arguments, environment, workspace view, network policy, and resource limits.

The MCP security guidance forbids token passthrough, says session IDs are not authentication, and recommends explicit user consent for local server startup, sandboxing, least privilege, and validation of URL schemes. [[MCP security best practices](https://modelcontextprotocol.io/docs/2025-11-25/tutorials/security/security_best_practices)] **Fact** Therefore “MCP server approved” cannot imply that every future tool or argument from that server is approved.

The official Rust SDK, `rmcp`, currently implements clients and servers with stdio, child-process, and Streamable HTTP transports. [[Official MCP Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)] [[`rmcp` README](https://github.com/modelcontextprotocol/rust-sdk/blob/main/crates/rmcp/README.md)] **Fact** Its roadmap reports a stable 3.x line and conformance work, but also notes that the conformance suite does not exercise every specification behavior. [[MCP Rust SDK roadmap](https://github.com/modelcontextprotocol/rust-sdk/blob/main/ROADMAP.md)] **Fact**

**Gate:** pin an exact `rmcp` and MCP specification version only during implementation, run the official MCP conformance suite, and add harness-specific adversarial tests. The official suite provides versioned client/server scenarios and wire validation, but passing it is a baseline rather than proof of security or complete semantics. [[MCP conformance suite](https://github.com/modelcontextprotocol/conformance)] **Fact** This updates the time-sensitive SDK assessment in the earlier Rust feasibility report without changing its architectural recommendation.

### 8.3 Native process tools

Tokio's process command inherits environment and current directory unless configured otherwise; it offers `env_clear`, current-directory control, Unix process groups, and `kill_on_drop`, but dropping a child does not kill it by default and Unix children still require reaping. [[Tokio `Command`](https://docs.rs/tokio/latest/tokio/process/struct.Command.html)] **Fact**

**Recommendation:** native tools default to direct executable plus argument-vector execution, never shell interpolation. Shell execution is a separately named, separately approved tool. The adapter must:

- resolve an approved absolute executable identity and, when policy requires, its digest;
- use an empty environment followed by an explicit allowlist;
- canonicalize a workspace-relative working directory and revalidate it at execution time;
- close unrelated file descriptors/handles and create dedicated stdin/stdout/stderr channels;
- inject secrets only through an explicitly authorized channel, never argv;
- install resource and containment controls before untrusted code starts;
- start draining stdout and stderr concurrently before waiting;
- report the exact installed enforcement, not merely requested settings.

Executable identity and working-directory resolution must follow the symlink and time-of-check/time-of-use rules in the deterministic protection report. Process supervision is operational containment; it does not replace the protection runtime's filesystem, network, credential, and syscall enforcement.

### 8.4 Plugin boundary

Native Rust dynamic libraries are a poor untrusted plugin ABI because they share process authority and Rust's native ABI has no stability guarantee. [[Rust Reference: ABI](https://doc.rust-lang.org/reference/items/external-blocks.html#abi)] **Fact** The repository's architectural report already prefers process protocols over native Rust ABI boundaries.

**Recommendation:** use these extension tiers:

1. built-in Rust adapters for trusted, latency-sensitive primitives;
2. supervised MCP or the narrow provider-sidecar protocol for independently released process plugins;
3. a later WASI Component Model backend for portable, capability-limited local plugins.

WIT defines typed component interfaces, and a component “world” declares its imports and exports; capabilities not imported are unavailable through that component interface. [[WIT design](https://component-model.bytecodealliance.org/design/wit.html)] [[Component Model worlds](https://component-model.bytecodealliance.org/design/worlds.html)] **Fact** Wasmtime provides the runtime, but its own security documentation still requires careful capability configuration and warns that untrusted terminal output can contain escape sequences. [[Wasmtime security](https://docs.wasmtime.dev/security.html)] **Fact**

**Gate:** do not introduce WASI until an actual plugin needs stronger in-process-like startup and portability than MCP provides. Spike filesystem/network capability mapping, resource limits, interruption, component versioning, and host-call output bounds first. Never load third-party native dynamic libraries into the engine process.

## 9. PTY, interactive, and background tools

### 9.1 Pipes are the default

A PTY changes program behavior: applications may enable color, cursor movement, alternate screens, line editing, and terminal escape sequences. POSIX terminal semantics also interact with sessions, controlling terminals, foreground process groups, and job-control signals. [[POSIX terminal interface](https://pubs.opengroup.org/onlinepubs/007904975/basedefs/xbd_chap11.html)] **Fact** Windows ConPTY requires the host to manage pseudoconsole input/output channels and uses UTF-8 streams. [[Windows Pseudoconsole](https://learn.microsoft.com/en-us/windows/console/pseudoconsoles)] **Fact**

**Recommendation:** pipe mode is the default tool contract. A tool requests PTY mode through an explicit `interactive_terminal` capability and declares terminal dimensions, input policy, inactivity deadline, and whether user attachment is required.

Keep three representations separate:

- raw bounded terminal bytes, retained only when policy permits;
- a terminal-emulator state used for a live client;
- a sanitized textual projection for model context, logs, search, and accessibility.

Never place raw terminal bytes directly into a web page, log viewer, or model prompt. Escape sequences and control characters are untrusted rendering instructions. PTY input becomes its own authorized event stream; do not let a detached agent impersonate live user keystrokes.

The Rust `portable-pty` crate offers a cross-platform PTY abstraction and is a plausible adapter dependency, not an architectural contract. [[`portable-pty`](https://docs.rs/portable-pty/latest/portable_pty/)] **Fact** **Gate:** test resize, UTF-8, EOF, signal delivery, handle cleanup, high-volume output, and maintenance health on every supported OS before selecting it.

### 9.2 Background jobs are durable entities

**Recommendation:** a background tool is never implemented by appending `&`, detaching, or returning while ownership becomes ambiguous. It creates a durable `Job` with:

- `JobId`, owning run/agent/effect, tool revision, and workspace reservation;
- supervisor identity and lease;
- process-tree or remote-operation identity;
- start time, deadline, heartbeat, and last-output cursor;
- bounded live output channel plus artifact references;
- attach/input permissions;
- current state and last confirmed cancellation/recovery action.

States should distinguish `Starting`, `Running`, `WaitingForInput`, `DetachedButSupervised`, `Stopping`, `Exited`, `Failed`, `Lost`, and `Uncertain`. On engine restart, the supervisor reconciles durable jobs with OS/container/remote state before accepting new commands. A lost local PID alone is not safe proof of identity because PIDs can be reused.

## 10. Process-tree supervision and isolation

### 10.1 Killing one PID is insufficient

On Linux cgroup v2, child processes inherit membership, and `cgroup.kill` can kill a cgroup and its descendants as a unit. [[Linux cgroup v2](https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html)] **Fact** Windows Job Objects group processes for limits, accounting, and whole-job termination; child processes are associated with the job by default, subject to configured breakaway behavior, and `KILL_ON_JOB_CLOSE` provides a cleanup mechanism. [[Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)] **Fact** Unix process groups help route signals, but are weaker containment because a process may create new sessions or otherwise escape when not prevented by a stronger sandbox. **Inference**

**Recommendation:** implement a platform `ProcessTree` abstraction with explicit strength:

- Linux: dedicated cgroup v2 plus process group, with resource limits and a verified `cgroup.kill` path where available;
- Windows: Job Object configured before child work begins, with nested/breakaway behavior tested and whole-job termination;
- macOS/other Unix: process group/session supervision as a weaker fallback, truthfully reported in enforcement attestation;
- remote/container runners: platform-native job identity and termination, verified through their control plane.

If policy requires strong descendant containment and the platform cannot install it, fail closed rather than display a generic “sandboxed” badge.

### 10.2 Cancellation ladder

**Recommendation:** tool cancellation follows a bounded ladder:

1. stop accepting model/user input and mark `CancelRequested`;
2. send protocol-level cancellation or close stdin when cooperative shutdown is supported;
3. signal the entire supervised tree for graceful termination;
4. after a configured grace period, kill the entire tree;
5. drain bounded remaining output, reap every child, close PTY/pipe handles, release workspace and secret leases;
6. record confirmed exit or `Uncertain`/`Lost` if the platform cannot prove it.

The ladder is driven by an injectable clock and remains active if a frontend disconnects. Engine shutdown either transfers supervision to a durable guardian or completes the ladder; it never abandons children by dropping a Rust handle.

### 10.3 Working directory, environment, and workspace

**Recommendation:** make execution context part of `EffectIntent`, not mutable ambient process state:

- `WorkspaceRevision` identifies the checkout/snapshot/reservation the approval saw;
- `cwd` is a validated relative path within that workspace;
- filesystem grants list allowed roots and modes;
- environment starts empty and names every injected value source;
- executable lookup never depends on an inherited, attacker-controlled `PATH`;
- temporary directories are per effect with quotas and deterministic cleanup policy;
- concurrent agents receive isolated workspaces or an explicit shared-workspace lease and conflict policy.

Canonical-path checks alone do not prevent later symlink replacement. Strong profiles require handle-relative/openat-style resolution or a mounted sandbox view as described in the deterministic protection report. The tool runtime consumes the resulting grant and enforcement attestation; it must not recreate authorization from strings.

## 11. Bounded output, artifacts, and backpressure

### 11.1 Output is an adversarial stream

A tool can write forever, omit newlines, interleave streams, emit enormous JSON, split UTF-8, or block because one pipe is not drained. MCP messages and progress notifications can produce equivalent pressure.

**Recommendation:** every execution declares independent limits for:

- stdout, stderr, terminal bytes, MCP message size, individual result size, aggregate result size, line length, decoded nesting, and event rate;
- in-memory preview, live subscriber lag, artifact bytes, artifact count, and total run storage;
- wall time, CPU, memory, process count, open files, and idle/input time.

The declared overflow policy is one of:

- `TruncatePreview`: continue execution, store a bounded artifact if allowed, and emit exact omitted-byte counts;
- `Terminate`: stop the effect because output beyond the limit violates its contract;
- `RejectResult`: let the process finish but mark the returned protocol value invalid;
- `Backpressure`: only when the producer protocol safely supports it and doing so cannot deadlock the child.

Never silently discard bytes. An output event carries stream identity, monotonic sequence/cursor, byte counts, truncation/gap markers, and optional artifact reference.

### 11.2 Artifacts

**Recommendation:** large results leave the event journal and enter a quota-controlled artifact store. An `ArtifactRef` contains an immutable digest, byte count, media type as observed and/or validated, origin effect, encryption/retention class, and safe display name. Clients fetch artifacts through authorization checks and bounded ranges.

Do not trust filenames, MIME declarations, archives, HTML, SVG, ANSI, or Markdown returned by a tool. Sanitize rendering, block path traversal, inspect archive expansion limits, and never execute or automatically open artifacts. Content-addressing deduplicates storage but must not create a cross-tenant existence oracle; access remains scoped by metadata.

## 12. Proposed Rust module boundaries

### 12.1 Start as modules, not a crate per box

The following are conceptual modules. Initially they can live behind private boundaries in a small number of crates. Split a crate only when dependency control, platform exclusion, compile locality, independent release, ownership, or test isolation provides a measured benefit.

```text
domain/
  model/             semantic request, output contracts, usage, capabilities
  effects/           EffectIntent, EffectId, approval binding, outcomes
  tools/             tool identity, definitions, invocation and job state

engine/
  provider_driver/   admission, deadlines, retry/resume/cancel, event sequencing
  effect_coordinator validation, policy, approval, outbox, recovery
  job_supervisor/    durable jobs, attachment, cancellation ladder

adapters/
  providers/         openai, anthropic, gemini, local/compatible
  tools/             native_process, mcp, wasi_later
  persistence/       event/effect/job stores

platform/
  http/              bounded streaming HTTP primitives
  credentials/       handles and short-lived leases
  process/           spawn, tree supervision, resource controls
  pty/               terminal mechanism and sanitized projection
  artifacts/         bounded immutable blobs
  clock/             real and deterministic time

telemetry/           secret-safe projections; no business decisions
```

The domain remains independent of Tokio, HTTP, JSON-RPC, MCP, provider schemas, SQL, PTY crates, operating-system handles, and OpenTelemetry types.

### 12.2 Provider interfaces

```rust
trait ProviderAdapter {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn capabilities(&self, target: &ModelTarget)
        -> Result<ProviderCapabilities, CapabilityError>;
    fn prepare(&self, request: &ModelRequest, caps: &ProviderCapabilities)
        -> Result<PreparedProviderRequest, ProviderPrepareError>;
    async fn invoke(&self, request: PreparedProviderRequest)
        -> Result<ProviderStream, ProviderStartError>;
    async fn resume(&self, continuation: ProviderContinuation)
        -> Result<ProviderStream, ProviderStartError>;
    async fn cancel(&self, operation: ProviderOperationId)
        -> ProviderCancellationResult;
}
```

Prepared wire requests are adapter-private and non-serializable outside encrypted diagnostic capture. The driver, not the adapter, owns the retry loop. Adapters classify failures and expose server hints. This prevents nested policies from multiplying attempts and makes user feedback consistent.

### 12.3 Effect and process interfaces

```rust
trait EffectStore {
    async fn append_intent_and_events(&self, tx: NewEffectTransaction) -> Result<(), StoreError>;
    async fn lease_ready(&self, worker: WorkerId, limit: usize) -> Result<Vec<EffectLease>, StoreError>;
    async fn finish(&self, lease: EffectLease, outcome: EffectOutcome) -> Result<(), StoreError>;
    async fn unfinished(&self) -> Result<Vec<RecoverableEffect>, StoreError>;
}

trait ProcessPlatform {
    async fn spawn(&self, spec: AuthorizedProcessSpec) -> Result<SupervisedProcess, SpawnError>;
    async fn signal_tree(&self, tree: ProcessTreeId, signal: TreeSignal) -> Result<(), ProcessError>;
    async fn wait(&self, tree: ProcessTreeId) -> Result<TreeExit, ProcessError>;
    async fn attach(&self, job: JobId, cursor: OutputCursor) -> Result<ProcessAttachment, ProcessError>;
    fn enforcement_report(&self, tree: ProcessTreeId) -> ProcessEnforcementReport;
}
```

Interfaces should expose semantic operations and failure states, not one method per syscall or provider endpoint. Bounded sinks and opaque prepared values keep difficult mechanics inside deep adapters.

## 13. Observability without a second source of truth

OpenTelemetry's generative-AI semantic conventions define provider, operation, model, token usage, and agent-related span concepts. [[OpenTelemetry GenAI spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-spans.md)] [[OpenTelemetry GenAI agent spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-agent-spans.md)] **Fact** The semantic-convention registry warns that prompts and completions are likely to contain sensitive information. [[OpenTelemetry GenAI attributes](https://github.com/open-telemetry/semantic-conventions/blob/main/docs/registry/attributes/gen-ai.md)] **Fact**

**Recommendation:** the durable journal is the source of truth; telemetry is a lossy, disposable projection. Useful spans include:

- `provider.prepare`, `provider.admission_wait`, `provider.call`, and `provider.stream`;
- `policy.evaluate`, `approval.wait`, `effect.lease`, and `effect.execute`;
- `process.spawn`, `process.terminate_tree`, `tool.mcp.call`, and `artifact.write`.

Useful metrics include queue wait, time to first provider event, inter-delta stalls, total latency, token usage, budget rejections, rate-limit waits, retry count, cancellation latency, output truncation, uncertain effects, orphan cleanup, process/resource limit terminations, and live-conformance age.

Attach provider/model/API version, adapter version, operation type, normalized failure class, retry number, and capability-degradation code. Request, run, effect, and job IDs belong in sampled spans/logs, not high-cardinality metric labels.

Prompts, completions, reasoning, tool arguments/results, environment values, terminal bytes, headers, and credentials are off by default. Optional content capture must be separately authorized, encrypted, size-bounded, retention-bounded, and visibly marked. Propagate trace context to trusted sidecars; never let arbitrary MCP metadata override local trace identity or sampling policy.

## 14. Evaluation and failure injection

### 14.1 Deterministic tests

Tokio supports pausing and advancing time for asynchronous tests, allowing deadlines and retries to be exercised without wall-clock sleeps. [[Tokio time testing](https://tokio.rs/tokio/topics/testing)] **Fact** Toxiproxy supplies deterministic latency, timeout, connection reset, bandwidth, and data-limit failures around networked systems. [[Toxiproxy](https://github.com/Shopify/toxiproxy)] **Fact**

**Recommendation:** use an injectable clock and seeded randomness for backoff, leases, and retry tests. Fault injection should cover:

- disconnect before headers, after headers, mid-SSE field, after tool arguments, and after terminal provider event;
- 429/reset hints, slowloris delivery, malformed or oversized fields, unknown events, duplicated events, and out-of-order events;
- crash before durable intent, after intent before dispatch, immediately after dispatch, after external success before commit, and during reconciliation;
- process stdout/stderr floods, no-newline output, blocked stdin, ignored graceful signal, forked descendants, daemonization, PTY resize/input races, and engine restart;
- MCP version/capability mismatch, invalid request IDs, notification floods, forged annotations, oversized results, remote redirects, cancellation races, and server restart/resumption;
- credential expiry/rotation, secret canaries in every diagnostic path, symlink replacement, workspace deletion, quota exhaustion, and artifact corruption.

### 14.2 Evaluation criteria

Every release candidate should measure or assert:

- claimed feature fidelity for each exact provider/model target;
- correct classification of malformed, partial, rate-limited, canceled, and unknown-outcome calls;
- no duplicate effect after each recoverable crash point;
- explicit `Uncertain` state when duplication cannot be excluded;
- bounded memory and disk under output and notification floods;
- process-tree cleanup and resource release within declared deadlines;
- zero secret-canary appearances in logs, traces, journals, previews, artifacts, and fixtures;
- deterministic approval invalidation when any bound intent field changes;
- responsiveness of live user feedback under many simultaneous provider and tool streams.

Live tests use dedicated low-privilege credentials, fixed budget ceilings, synthetic content, and provider-specific cleanup. They must never import a developer's normal secret store or record third-party production data.

## 15. Phase ordering and gates

### Phase 0: freeze semantics before mechanisms

Define the capability vocabulary, normalized event grammar, provider failure taxonomy, `EffectIntent`, approval binding, outbox state machine, idempotency contracts, output limits, and cancellation outcomes. Record the durable decisions in architecture documentation or ADRs.

**Pre-implementation evidence gates:**

- verify feature and schema claims against the exact configured provider API versions and model snapshots;
- use Linux as the first strong platform and spike process-tree containment there;
- instantiate the credential and raw-content threat models already defined by the repository security rules and protection research;
- set initial queue, stream, output, artifact, retry, and retention limits;
- specify canonical JSON/value rules and approval digest format.

No production adapter should precede this minimal semantic contract, but avoid designing every future provider in advance.

### Phase 1: one provider end to end

Implement one direct Rust provider adapter with streaming text, complete tool calls, strict structured output where supported, opaque reasoning continuity/provider summaries, final usage, admission control, cancellation, fixtures, and an opt-in live probe. Wire semantic milestones into the existing run reducer and coalesced deltas into the feedback plane.

**Implementation gate:** success, malformed stream, partial-stream failure, rate limit, unsupported capability, cancellation, secret-canary, and bounded-buffer tests pass. Measure time to first event and memory under sustained deltas. Do not add a universal provider framework beyond what this adapter and the next known provider actually require.

### Phase 2: durable native effects

Implement effect canonicalization, policy/approval binding, transactional outbox, native pipe-mode execution, bounded output/artifacts, Linux process-tree supervision, recovery, and the cancellation ladder.

**Implementation gate:** inject a crash at every outbox transition; prove idempotent effects recover without duplication and non-idempotent ambiguous effects become `Uncertain`. Prove descendants, pipes, file handles, workspace leases, and secret leases are cleaned up. Complete the security pass from the deterministic protection report.

### Phase 3: MCP and durable jobs

Add a pinned `rmcp` adapter, stdio first, official conformance, hostile-server tests, and durable background jobs. Add Streamable HTTP only with the remote authentication and transport threat model complete.

**Version-refresh gate:** re-check the current MCP specification, Rust SDK version, conformance requirement set, authentication guidance, and unresolved SDK deviations at implementation time. The ecosystem is moving too quickly for this report's version observations to become repository law.

### Phase 4: provider breadth and velocity experiment

Add a second native provider adapter to validate the abstraction, then a third only when product need justifies it. Run one time-boxed official-SDK sidecar experiment against a feature known to lag in Rust.

**Decision gate:** keep the sidecar only if it materially reduces verified feature lag while meeting latency, memory, deployment, credential-isolation, cancellation, and observability budgets. Otherwise retain it solely as a test oracle or remove it.

### Phase 5: interactive and portable plugins

Add PTY execution, Windows Job Object and ConPTY support, and a WASI Component Model plugin spike in that order only when demanded by a real tool or client.

**Triggered future gates:**

- determine what strong process-tree and filesystem/network containment is achievable on macOS and which guarantees must be weakened;
- validate PTY crate behavior and raw-terminal rendering protections on every supported OS;
- map WASI capabilities to the harness grant model and prove interruption/resource bounds;
- decide artifact encryption, retention, archive handling, and tenant-isolation requirements before remote/multi-user deployment.

## 16. Decisions and remaining executable gates

### Recommended decisions now

1. Rust remains the provider and tool-runtime implementation language by default.
2. Provider normalization is layered: portable semantics, explicit capabilities, typed provider options, and a bounded extension escape hatch.
3. The engine owns retry, cancellation, admission, effect identity, approval, and recovery policy; adapters own protocol mechanics.
4. Direct Rust HTTP is the production default; an official-SDK sidecar is an evidence-driven fallback and conformance oracle.
5. Tool calls become durable, policy-bound effects before dispatch.
6. Exactly-once is never claimed for an arbitrary external tool; idempotency, reconciliation, and `Uncertain` are explicit contracts.
7. Native process, MCP, later WASI, PTY, and background execution share lifecycle semantics but retain separate adapters.
8. Process-tree supervision and sandbox enforcement are separate, composable responsibilities.
9. The event journal contains semantic milestones; high-frequency deltas are bounded and coalescible; artifacts hold large output.
10. Telemetry never becomes a second state store and never captures sensitive content by default.

### Closed defaults and evidence still required

| Area | Version-1 default | Evidence or trigger |
|---|---|---|
| Provider matrix | Deterministic fake first, OpenAI first real adapter, Anthropic second; Gemini is the third cloud-shape validation when required. Exact model IDs are trusted configuration, not Domain constants. | Recorded and opt-in live conformance for every advertised provider/model/API revision. |
| Provider-side recovery | Assume no server-side idempotency, operation lookup, cancellation acknowledgement, or resume guarantee unless current capability evidence proves it for the exact endpoint. | Adapter contract probe; unsupported or unknown capabilities remain unavailable. |
| macOS containment | Do not advertise the Linux strict profile. Use a managed/remote Linux or VM execution tier for strict work until native conformance passes. | Same descendant, filesystem, network, secret, and cleanup corpus as Linux. |
| Artifact protection | Local single-user MVP uses content-addressed immutable bytes, access checks, sensitivity/retention labels, and explicit deletion/GC. Multi-tenant encryption and tenant isolation are outside version 1. | A remote or hosted product triggers a new threat model and storage design. |
| PTY/background tools | Pipe-mode foreground tools are the default. PTY or persistent ownership is enabled only by an explicitly declared tool requirement and a user-visible capability grant. | A named tool scenario plus input, detach, reconnect, cancel, sanitize, and retention tests. |
| SDK sidecar | Direct Rust HTTP remains production default. | Keep a sidecar only when a measured feature-lag incident shows material benefit within latency, deployment, credential, cancellation, and observability budgets. |
| WASI plugins | Supervised MCP/process extensions are sufficient for the initial product; no WASI host ships in version 1. | A real extension requires lower-latency in-process sandboxing and a stable capability-oriented WIT interface passes security review. |

These rows are implementation gates or explicitly triggered future scope, not missing architectural research. They do not delay the semantic boundaries that make each experiment replaceable.
