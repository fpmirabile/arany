# Beta multi-provider routing and adapters

> **Custom-profile amendment — 2026-09-29:** [Beta Sessions, teams, terminal, providers, and license](./beta-sessions-teams-terminal-providers-and-license.md) keeps native OpenAI and Anthropic but moves a built-in OpenRouter profile behind its later broker gate. Beta extensibility instead admits exact user-owned protocol/endpoint/model profiles only after a data-free conformance check proves strict outcomes, server-side output limits, route identity, bounds, cancellation, and safe provenance. Arany never claims universal “OpenAI-compatible” support. Provider/model may change between Session Runs, never within one.

> **Authentication amendment — 2026-09-29:** [Consumer subscription authentication](./consumer-subscription-authentication-for-provider-adapters.md) keeps the beta API-key only. OpenAI's official `Using ChatGPT plan` design is the sole future subscription candidate, but its current inference route rejects the provider-enforced output cap required by Arany's aggregate Run budget; automatic browser opening also needs a narrow trusted-process decision. Anthropic remains API-key only without prior approval. Arany does not import another CLI's tokens, copy its OAuth client, call private ChatGPT routes, or ship dormant login code.

Retrieved: 2026-09-29

Scope: how Arany can switch among native OpenAI and Anthropic adapters and exact user-owned custom profiles without weakening its bounded `Delegate | Finish` workflow, its single `Provider` seam, or its origin-bound security model. OpenRouter and Z.AI are evaluated as future built-in profiles. Only first-party provider documentation and policies are used.

## Executive decision

**Fact:** The four named services do not all document a native mechanism that can enforce Arany's `Delegate | Finish` JSON Schema. OpenAI Structured Outputs and Anthropic structured outputs do. OpenRouter can do so only when the selected model *and actual upstream endpoint* support structured outputs. Z.AI documents JSON mode, which guarantees JSON syntax, followed by client-side JSON Schema validation; it does not document provider-enforced schema adherence. Z.AI also documents only `auto` tool choice, so a forced strict tool call is not an alternative. ([OpenAI structured outputs](https://developers.openai.com/api/docs/guides/structured-outputs), [Anthropic structured outputs](https://platform.claude.com/docs/en/build-with-claude/structured-outputs), [OpenRouter structured outputs](https://openrouter.ai/docs/guides/features/structured-outputs), [Z.AI structured output](https://docs.z.ai/guides/capabilities/struct-output), [Z.AI function calling](https://docs.z.ai/guides/capabilities/function-calling))

**Bottom line:** No, all four named providers cannot satisfy the current beta contract natively on the evidence available. The built-in beta product set is `openai` and `anthropic`. Exact user-owned custom profiles may use one explicitly supported protocol family, starting with `openai-responses`, only after data-free conformance succeeds. A built-in constrained `openrouter` profile is deferred behind its broker/privacy gate; `zai` remains gated until Z.AI documents strict JSON Schema output or strict, forceable tool inputs. Provider switching belongs in beta, but prompt-only or locally repaired JSON does not.

| Adapter | Contract status | Allowed beta encoding | Release gate |
| --- | --- | --- | --- |
| OpenAI | Native | Responses `text.format.type = json_schema`, strict schema | A pinned model passes fixtures and opt-in live direct/team Runs |
| Anthropic | Native | Messages `output_config.format.type = json_schema` | A pinned model passes fixtures and opt-in live direct/team Runs |
| Exact custom profile | Conditional, user-owned | One named supported protocol family with exact origin/model/capabilities | Data-free conformance proves strict output, bounds, cancellation, and provenance before Workspace disclosure |
| OpenRouter | Conditional broker capability; built-in profile deferred | `response_format.type = json_schema` with a pinned model and upstream route | Broker privacy/routing gate and live conformance pass in a later milestone |
| Z.AI | Unsupported for the current contract | None; `json_object` plus local validation is not strict output | Official strict-schema or strict-forced-tool capability appears and passes conformance |

**Recommendation — exact fallback policy:**

1. Prefer provider-enforced JSON Schema response output.
2. If and only if a provider/model officially documents both strict tool-argument schema enforcement and forced tool selection, the adapter may instead expose one synthetic, non-effectful `submit_agent_outcome` tool, force exactly that tool, disable parallel tool calls, and require exactly one call. The arguments use the same `Delegate | Finish` schema and are validated again locally. The tool is decoded, never executed.
3. Never fall back from strict response output to a tool call merely because a request failed. The encoding is selected before `RunStarted` from a tested capability snapshot.
4. Reject the run before `RunStarted` if the selected route offers only JSON mode, prompt instructions, `auto` tool selection, or client-side repair. A refusal, truncation, missing tool call, duplicate tool call, or schema mismatch is one failed Provider call; it is not retried through another encoding or provider.

OpenAI documents forced functions and `strict: true` tool schemas. Anthropic documents strict tools, but also model/settings combinations where forced tool choice is rejected, so the fallback is model-specific rather than an Anthropic-wide promise. Z.AI documents only `tool_choice: auto`. ([OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling), [Anthropic tool choice](https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools), [Z.AI function calling](https://docs.z.ai/guides/capabilities/function-calling))

## What “smart switching” should mean

**Inference:** Opaque failover during a run is incompatible with the current proof. It can change the model semantics, data processor, retention policy, price, cache behavior, and response encoding after some canonical facts already exist. OpenRouter itself defaults to price-oriented load balancing and enables upstream fallback by default. ([OpenRouter provider routing](https://openrouter.ai/docs/guides/routing/provider-selection))

**Recommendation:** In beta, “smart” means capability-aware and deterministic selection *before* the run:

- the user explicitly selects one adapter and model;
- Arany resolves that choice to a tested capability snapshot and a compiled endpoint profile;
- the Engine pins the resolved adapter, model, schema encoding, reasoning mode, privacy policy, and route identity for the admitted Run call budget;
- unsupported combinations fail before workspace discovery, credential loading for unrelated providers, or `RunStarted`;
- no cross-provider or cross-model failover occurs inside the run.

This preserves the admitted call formula: one Provider call for a direct answer, or `N + 2` calls for a team Run with `N` children. Provider discovery or conformance checking is an explicit data-free command, never a hidden extra request after `RunStarted`.

**Implementation gate:** Automatic cross-provider routing is outside this beta. It requires a separately approved policy with price and privacy ceilings, canonical attempt facts, a retry/idempotency model, and evaluation evidence that mixed-provider Runs remain meaningful.

## Official API surfaces and endpoint trust

### OpenAI

**Fact:** The native surface is `POST https://api.openai.com/v1/responses`, authenticated with a bearer API key conventionally loaded from `OPENAI_API_KEY`. Responses supports strict structured text output, tools, streaming, reasoning controls, and usage. `GET /v1/models` lists model IDs available to the project, but availability does not itself prove structured-output or reasoning capability. ([Responses API](https://developers.openai.com/api/reference/resources/responses/methods/create), [models list](https://developers.openai.com/api/reference/resources/models/methods/list), [quickstart](https://developers.openai.com/api/docs/quickstart))

**Recommendation:** Keep the existing direct Responses adapter, `store: false`, non-streaming behavior, bounded body/time limits, disabled redirects/cookies/ambient proxies, and compiled origin. Keep `ARANY_OPENAI_MODEL`; do not infer a default from the models list.

### Anthropic Claude

**Fact:** The native surface is `POST https://api.anthropic.com/v1/messages`, using `x-api-key` and an `anthropic-version` header. Official SDKs read `ANTHROPIC_API_KEY`. `GET /v1/models` lists models available to the API key. ([Messages create](https://platform.claude.com/docs/en/api/messages/create), [models list](https://platform.claude.com/docs/en/api/models/list), [API overview](https://platform.claude.com/docs/en/api/overview))

Anthropic also offers an OpenAI SDK compatibility layer at `https://api.anthropic.com/v1/`, but explicitly positions it for testing and comparison rather than a long-term production solution. On that surface, `strict`, `response_format`, `store`, `reasoning_effort`, and prompt-caching behavior are ignored or translated incompletely. ([Anthropic OpenAI SDK compatibility](https://platform.claude.com/docs/en/api/openai-sdk))

**Recommendation:** Implement Anthropic through native Messages. Do not create a generic “OpenAI-compatible provider” adapter and point it at Anthropic; that would silently erase the exact capabilities beta depends on. Use a compiled global origin. If US-only inference is offered, express it through Anthropic's documented `inference_geo` request control, not a user-entered URL. ([Anthropic data residency](https://platform.claude.com/docs/en/manage-claude/data-residency))

### Z.AI

**Fact:** Z.AI's international Model API exposes an OpenAI-style Chat Completions surface at `POST https://api.z.ai/api/paas/v4/chat/completions`, authenticated with a bearer token. Its official OpenAI SDK guide uses `ZAI_API_KEY` and `https://api.z.ai/api/paas/v4/` as the base URL. The Chat Completion reference currently enumerates model IDs such as `glm-5.3`, `glm-5.2`, and `glm-5.1`; it also returns an `id`, `request_id`, model, token usage, cached-token usage, visible content, and `reasoning_content`. ([Z.AI HTTP API](https://docs.z.ai/guides/develop/http/introduction), [OpenAI SDK compatibility](https://docs.z.ai/guides/develop/openai/python), [Chat Completion reference](https://docs.z.ai/api-reference/llm/chat-completion))

**Inference:** The API is wire-familiar, not semantically equivalent. Z.AI's official documentation is the authority for supported fields. In particular, its structured-output and tool-choice limitations prevent an OpenAI adapter from being reused safely.

**Recommendation:** A future Z.AI adapter should use the fixed international Model API origin above, not the separate Coding Plan endpoint and not an arbitrary compatible base URL. Treat any other region or commercial surface as a separate compiled endpoint profile with separately reviewed credential and data-policy semantics. Use an explicit model ID; do not assume that the reference's default remains suitable.

### OpenRouter

**Fact:** OpenRouter offers OpenAI-compatible Chat Completions at `https://openrouter.ai/api/v1/chat/completions` and an OpenResponses endpoint at `https://openrouter.ai/api/v1/responses`, authenticated with `OPENROUTER_API_KEY` as a bearer token. `GET /api/v1/models` supplies model slugs, while provider and endpoint metadata determine whether a particular route supports a parameter. App-attribution headers are optional and make the application visible in OpenRouter analytics/rankings, so a CLI need not send them. ([OpenRouter quickstart](https://openrouter.ai/docs/quickstart), [Responses create](https://openrouter.ai/docs/api/api-reference/responses/create-responses), [models API](https://openrouter.ai/docs/api/api-reference/models/get-models), [app attribution](https://openrouter.ai/docs/app-attribution))

**Recommendation:** Treat OpenRouter as its own broker adapter, not an OpenAI alias. For beta, use one compiled API style, preferably Chat Completions because provider routing controls are explicitly documented there. Pin a namespaced model slug and one exact upstream endpoint slug; send:

```json
{
  "provider": {
    "only": ["<reviewed-endpoint-slug>"],
    "allow_fallbacks": false,
    "require_parameters": true,
    "data_collection": "deny",
    "zdr": true
  }
}
```

`zdr: true` is a routing constraint, not a claim that every model is available. Failure to find a qualifying endpoint must fail closed. OpenRouter documents that `require_parameters: false` can allow an upstream to ignore unknown parameters, that `data_collection` defaults to `allow`, and that fallback defaults to enabled. ([OpenRouter provider routing](https://openrouter.ai/docs/guides/routing/provider-selection))

OpenRouter's Business and Enterprise regional origins are `https://eu.openrouter.ai` and `https://us.openrouter.ai`; the global origin remains a distinct trust profile. Region selection may choose one of those compiled origins, but must never accept an arbitrary URL. ([OpenRouter provider routing](https://openrouter.ai/docs/guides/routing/provider-selection), [OpenRouter in-region announcement](https://openrouter.ai/blog/announcements/us-in-region-routing/))

## Structured outcomes and capability negotiation

**Fact:** OpenAI distinguishes JSON mode, which guarantees valid JSON, from Structured Outputs, which enforces its supported JSON Schema subset. Strict function tools similarly require every property to be required and `additionalProperties: false`. ([OpenAI structured outputs](https://developers.openai.com/api/docs/guides/structured-outputs), [OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling))

**Fact:** Anthropic's native `output_config.format` supports constrained JSON Schema output. Refusal and `max_tokens` termination still require explicit handling, and schema compilation has its own cache/retention behavior. ([Anthropic structured outputs](https://platform.claude.com/docs/en/build-with-claude/structured-outputs))

**Fact:** OpenRouter says structured-output support is per endpoint, not merely per model, can change over time, and should be constrained with `require_parameters: true`. ([OpenRouter structured outputs](https://openrouter.ai/docs/guides/features/structured-outputs))

**Fact:** Z.AI's `response_format: {"type":"json_object"}` documentation instructs the developer to describe the shape in the prompt and demonstrates local validation with a JSON Schema library. It does not expose `json_schema`, `strict`, or forced tool selection. ([Z.AI structured output](https://docs.z.ai/guides/capabilities/struct-output), [Z.AI function calling](https://docs.z.ai/guides/capabilities/function-calling))

**Recommendation:** Capabilities must be affirmative and model/route-scoped, not a lowest-common-denominator boolean. A small closed representation is sufficient:

```text
CapabilitySnapshot {
  adapter, endpoint_profile, requested_model, resolved_model,
  outcome_encoding: StrictResponseSchema | StrictForcedTool,
  streaming: Supported | Unsupported | Unknown,
  reasoning: provider-specific reviewed profile,
  prompt_cache: provider-specific reviewed profile,
  privacy: provider-specific reviewed profile,
  evidence_version
}
```

Missing, unknown, or stale evidence does not become `false` and trigger a weaker mode; it blocks only the feature that requires it. Model catalog endpoints are discovery aids, not capability or privacy authorities. OpenRouter needs both catalog metadata and a live route check because its endpoint support changes independently of the model slug. Capability snapshots should be shipped/tested manifests, refreshed deliberately, and captured in the canonical run facts.

### Tool calling remains deferred

**Fact:** The wire protocols overlap but do not establish one tool contract. OpenAI supports forced, required, and automatic function selection plus strict argument schemas. Anthropic uses `tool_use` content blocks and supports strict tools, while forced selection varies by model and thinking mode. Z.AI exposes OpenAI-shaped tools but documents only `tool_choice: auto`. OpenRouter normalizes tool calling only for compatible model endpoints, and without `require_parameters: true` it can route a request where an unsupported parameter is ignored. ([OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling), [Anthropic tool use](https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools), [Z.AI function calling](https://docs.z.ai/guides/capabilities/function-calling), [OpenRouter tool calling](https://openrouter.ai/docs/guides/features/tool-calling), [OpenRouter provider routing](https://openrouter.ai/docs/guides/routing/provider-selection))

**Recommendation:** The beta adapters parse no effectful tool requests and expose no Engine tool API. The one synthetic `submit_agent_outcome` call allowed by the fallback policy is only a constrained response encoding; its arguments are never dispatched. Before the first effectful tool ships, add the already-planned deterministic Policy and separate Guard, explicit authority and result types, bounded output, per-provider tool fixtures, and failure/cancellation semantics. Do not let provider-native server tools bypass that boundary.

**Implementation gate:** General tool capability negotiation waits for an approved effectful-tool design. It must be richer than a `supports_tools` flag because strictness, forced selection, parallelism, streaming arguments, server-side execution, and thinking interactions are independent properties.

## Smallest deep Provider interface

The one-seam rule still holds. The deterministic fake plus the existing OpenAI adapter already justify the `Provider` seam; adding adapters deepens that same module rather than adding Engine seams.

**Recommendation:** Keep the existing Engine-facing interface unchanged and semantic. Adapter selection and capability validation happen while constructing `P`; Engine cancellation continues to race and drop the returned future rather than becoming a Provider parameter:

```rust
trait Provider: Send + Sync + 'static {
    fn invoke(
        &self,
        request: ProviderRequest,
    ) -> impl Future<Output = Result<ProviderResponse, ProviderError>> + Send;
}
```

`ProviderRequest` carries the workflow phase, resolved instructions/context, the canonical `Delegate | Finish` schema, and an output bound. `ProviderResponse` carries the locally validated domain outcome plus bounded provenance and usage. The constructed adapter privately owns `ResolvedProvider` and stamps its capability snapshot into each response; capability discovery is therefore not another trait method invoked unpredictably during the run.

The trait must not expose OpenAI messages, Anthropic content blocks, OpenRouter routing JSON, Z.AI `reasoning_content`, raw SSE, or a generic parameter bag. It must not grow tool execution now: the synthetic strict-output tool, where allowed, is a private adapter encoding, not an Engine tool.

**Recommendation — adapter organization:** Keep `provider.rs` while the implementations are shallow. Split private `provider/openai.rs`, `provider/anthropic.rs`, `provider/openrouter.rs`, and `provider/zai.rs` only when their parsing and error mapping gain real depth. Share one concrete, private hardened HTTP client builder, bounded body reader, redaction helpers, and possibly a tested SSE parser. Do not introduce a `Transport` trait: there is not a second behavior implementation. Each adapter owns its authentication, request document, response state machine, error taxonomy, schema encoding, and usage mapping. Similar JSON fields are not shared semantics.

## Configuration and CLI contract

**Recommendation:** Add only these product-facing selectors:

```text
arany ... --provider <openai|anthropic|openrouter> --model <MODEL_ID>

ARANY_PROVIDER
ARANY_OPENAI_MODEL
ARANY_ANTHROPIC_MODEL
ARANY_OPENROUTER_MODEL
ARANY_ZAI_MODEL                 # reserved while the adapter is gated

OPENAI_API_KEY
ANTHROPIC_API_KEY
OPENROUTER_API_KEY
ZAI_API_KEY                     # read only by an explicitly enabled Z.AI preview
```

Precedence is `--provider` over `ARANY_PROVIDER`; the selected provider's `--model` over its provider-specific model variable; otherwise configuration fails. There is no default provider or model. Keeping provider-specific model variables prevents changing `ARANY_PROVIDER` from silently reinterpreting a stale generic model. `ARANY_OPENAI_MODEL` therefore remains valid rather than being renamed.

Only the selected adapter's credential is read, after trusted CLI/config resolution and before workspace input. API keys are never accepted as CLI arguments or config-file values. Add no `--base-url`, generic compatibility mode, proxy flag, routing URL, or provider-header escape hatch. A narrowly typed `--region <global|eu|us>` may select only reviewed compiled profiles and must reject provider/region combinations that do not exist.

**Recommendation:** Do not expose Z.AI in the normal provider enum until its contract gate passes. If implementation learning requires a preview, require a conspicuous build feature or `--experimental-provider zai`; never silently run it under `--provider zai` while claiming strict outcomes.

An explicit, non-run command may later retrieve catalogs, for example `arany provider models --provider openrouter`, but it must label results as availability rather than compatibility. It must apply the same egress and secret rules and cannot mutate run configuration.

## Streaming, cancellation, and reasoning

**Fact:** All four services document SSE streaming. Anthropic warns that an error can occur after an initial HTTP 200 and that clients must tolerate unknown event types. Z.AI ends standard streams with `[DONE]` and can stream `reasoning_content`. OpenRouter normalizes streaming across upstreams. OpenAI also offers cancellable background Responses, but background mode retains response state for roughly ten minutes even with `store: false`. ([OpenAI streaming](https://developers.openai.com/api/docs/guides/streaming-responses), [OpenAI background mode](https://developers.openai.com/api/docs/guides/background), [Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming), [Z.AI streaming](https://docs.z.ai/guides/capabilities/streaming), [OpenRouter streaming](https://openrouter.ai/docs/api/reference/streaming))

**Recommendation:** Preserve non-streaming beta calls. Cancellation aborts the local HTTP request and records `cancel requested`; it must not claim remote cancellation or zero billing without provider confirmation. Do not use OpenAI background mode because its state semantics conflict with the current minimal `store: false` posture. When streaming is later added, each adapter maps its own event grammar into one bounded internal accumulator; no partial `Delegate | Finish` value becomes canonical.

**Fact:** Reasoning controls differ materially. OpenAI exposes model-dependent reasoning effort and optional summaries while billing/counting reasoning tokens. Anthropic has model-dependent adaptive/manual thinking and effort, with special tool-choice interactions. Z.AI uses `thinking.type`, model-specific `reasoning_effort`, and returns `reasoning_content`; current GLM-5.3 documentation says thinking cannot be disabled. OpenRouter normalizes a subset while the actual upstream remains relevant. ([OpenAI reasoning](https://developers.openai.com/api/docs/guides/reasoning), [Anthropic extended thinking](https://platform.claude.com/docs/en/build-with-claude/extended-thinking), [Anthropic effort](https://platform.claude.com/docs/en/build-with-claude/effort), [Z.AI deep thinking](https://docs.z.ai/guides/capabilities/thinking), [OpenRouter reasoning tokens](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens))

**Recommendation:** Do not add one misleading cross-provider `reasoning_effort` switch in beta. Pin a reviewed per-model reasoning profile inside the capability snapshot. Persist the requested/effective control and usage counts, but not raw chain-of-thought. A provider-supported summary may be bounded and treated as lossy observability, never canonical state.

## Usage, cost, caching, and privacy provenance

**Recommendation:** Every Provider call fact should preserve, within existing bounds:

- adapter, endpoint profile, requested and returned/resolved model;
- broker and actual upstream endpoint/provider where applicable;
- provider request/generation ID;
- input, output, reasoning, and cached/cache-write token fields when reported;
- service tier and finish/incomplete reason;
- provider-reported monetary cost and currency when reported, otherwise `unknown` rather than a reconstructed “actual” cost;
- privacy/routing controls actually requested.

Direct provider usage is authoritative only for the fields returned. A locally computed price from a catalog is an estimate tied to a dated price snapshot. OpenRouter can include cost in usage and can expose routing metadata when `X-OpenRouter-Metadata: enabled`; this avoids a fifth generation lookup. The adapter must retain broker and upstream provenance, not collapse both to the requested model slug. ([OpenRouter live usage accounting](https://openrouter.ai/blog/announcements/smarter-charts-inline-svgs-and-live-usage-accounting/), [OpenRouter Chat metadata](https://openrouter.ai/docs/api/api-reference/chat/send-chat-completion-request), [OpenRouter generation metadata](https://openrouter.ai/docs/api/api-reference/generations/get-generation))

**Fact:** Prompt caching is not uniform. OpenAI automatically performs prefix-based prompt caching on supported models and reports cached tokens. Anthropic supports automatic or explicit cache control with provider-defined TTLs and separate cache creation/read usage. Z.AI documents implicit context caching and cached tokens. OpenRouter's prompt caching depends on the upstream and can affect route stickiness. ([OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching), [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching), [Z.AI context caching](https://docs.z.ai/guides/capabilities/cache), [OpenRouter prompt caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching))

**Recommendation:** Add no explicit cache controls in the first multi-provider increment. Preserve stable prompt prefixes where natural, record reported cache usage, and document provider-default retention. OpenRouter response caching must remain disabled: it stores and replays whole successful responses at the broker layer and reports zero billable usage on a hit, which would obscure inference provenance; it is also incompatible with account-level ZDR. ([OpenRouter response caching](https://openrouter.ai/docs/guides/features/response-caching))

**Fact:** OpenAI API data is not used to train models by default; default abuse-monitoring logs may be retained for up to 30 days, while `store: false` controls Responses application state but is not itself Zero Data Retention. Anthropic's API retention depends on product, model, organization agreements, and optional ZDR; prompt and structured-schema caches have separate retention. Z.AI's API data-processing terms state that API content is processed in real time and not saved, while other categories and legal exceptions remain; this should be verified against the customer's applicable agreement. OpenRouter sends content to the selected upstream and its `data_collection`/`zdr` routing filters depend on OpenRouter's endpoint metadata rather than replacing the upstream policy. ([OpenAI data controls](https://developers.openai.com/api/docs/guides/your-data), [Anthropic API data retention](https://platform.claude.com/docs/en/manage-claude/api-and-data-retention), [Z.AI privacy policy and API terms](https://docs.z.ai/legal-agreement/privacy-policy), [OpenRouter provider routing](https://openrouter.ai/docs/guides/routing/provider-selection), [OpenRouter privacy policy](https://openrouter.ai/privacy))

**Recommendation:** Documentation and run provenance must distinguish “storage disabled in this request,” “endpoint classified ZDR,” “organization has a ZDR agreement,” and “provider default.” They are not interchangeable. OpenRouter adds a broker and potentially a second processor; even a direct-provider-equivalent model therefore has a different trust boundary.

## Errors, request IDs, rate limits, retries, and idempotency

**Fact:** Providers expose different error envelopes and rate-limit dimensions. Anthropic documents request IDs on responses, HTTP 429/5xx/529 classes, `retry-after`, and SDK retries for transient errors. OpenAI documents request IDs, rate-limit headers, and exponential backoff guidance. Z.AI documents HTTP/business error codes and returns `request_id` in successful Chat Completion responses. OpenRouter has broker errors plus upstream errors and generation IDs. ([Anthropic errors](https://platform.claude.com/docs/en/api/errors), [Anthropic rate limits](https://platform.claude.com/docs/en/api/rate-limits), [OpenAI rate limits](https://developers.openai.com/api/docs/guides/rate-limits), [OpenAI error codes](https://developers.openai.com/api/docs/guides/error-codes), [Z.AI API error codes](https://docs.z.ai/api-reference/api-code), [OpenRouter Chat errors](https://openrouter.ai/docs/api/api-reference/chat/send-chat-completion-request))

**Recommendation:** Normalize only the Engine decision classes—configuration, authentication, permission, invalid request/capability, rate limited, timeout, remote unavailable, malformed response, refused, truncated, cancelled, and internal—while retaining bounded provider code, HTTP status, request ID, and safe retry hint. Never persist response bodies, credentials, prompts, or uncontrolled headers in diagnostics.

Keep the current no-automatic-retry rule. A generation can consume tokens and create provider state even when the client loses the response; there is no documented inference-wide idempotency key common to these surfaces. Failed rate-limited attempts can also count against limits. Any broker fallback must be disabled because it is an implicit retry at a different processor. A future retry policy needs provider-specific idempotency evidence, an attempt ledger, aggregate time/call/cost bounds, jitter, `Retry-After` handling, and an explicit decision about how an extra attempt consumes the admitted Run budget.

## Verification map

The existing strict scripted Provider remains the sole Engine behavior double and proves direct and bounded-team workflows. Adapter and custom-profile conformance are different layers.

**Recommendation — offline fixtures:** Add minimal, hand-reviewed fixtures derived from official examples for each adapter. They must cover a successful outcome, refusal, truncation, malformed/extra fields, wrong phase/outcome, body limit, timeout classification, safe request-ID extraction, usage/cache/reasoning mapping, and provider-specific errors. OpenRouter fixtures also cover missing or unexpected upstream provenance and prove the exact routing object. Z.AI has a negative conformance fixture proving that JSON mode cannot be advertised as strict schema support. Do not add a configurable production base URL or test HTTP server merely to intercept the real adapter.

**Recommendation — live tests:** Keep live tests ignored and require both an explicit activation flag and credentials; the presence of a key alone never spends money. Native OpenAI and Anthropic each prove a direct Run and one small bounded team Run through the shipped executable with a caller-supplied model and canonical provenance checks. A custom profile first passes a data-free conformance command; any paid live Run is a separate explicit action. Future OpenRouter and Z.AI probes remain explicitly paid experimental tests and must not imply beta compatibility.

For example, require `ARANY_RUN_LIVE_PROVIDER_TESTS=1` plus the selected provider's key and model variable. CI never enables it by default. Redaction canaries, hostile network fixtures, formatting, linting, the deterministic integration journey, all adapter fixtures, and a final diff review remain release gates.

## Staged implementation and release gates

1. **Stage 0 — preserve the proof.** Refactor provider construction into `ResolvedProvider` plus the unchanged Provider trait behavior. Keep the scripted fake, strict schema, dynamic admitted call budget, fixed native origins, non-streaming behavior, `store: false` where supported, and no retries. **Gate:** no behavioral diff in the deterministic Session journey or OpenAI live smoke.
2. **Stage 1 — Anthropic native.** Add `--provider`, native Messages structured output, Anthropic error/usage mapping, and provider-specific configuration. This is the second product adapter that proves switching without a broker. **Gate:** fixtures and `live_anthropic_team_run` pass with the same domain outcomes.
3. **Stage 2 — exact custom profiles.** Add trusted user configuration for one named protocol family, exact origin, model, credential reference, and capability evidence. **Gate:** a data-free conformance command proves strict output, server-enforced output limits, cancellation, safe provenance, and origin identity before any Workspace data is sent.
4. **Stage 3 — broker and Z.AI gates.** Evaluate a built-in constrained OpenRouter profile and Z.AI separately. OpenRouter must pin routing/privacy behavior and capture the actual upstream; Z.AI must document provider-enforced JSON Schema or strict forceable tool arguments. Neither is a beta compatibility claim until its gate passes.

**Exact recommended beta set after the staged gates:** deterministic fake for tests; native `openai` and `anthropic`; and conformance-gated exact custom profiles using explicitly supported protocol families. Built-in `openrouter` and `zai` profiles remain outside beta compatibility claims.

## Reconciliation with current repository decisions

| Current assumption | Decision after this research |
| --- | --- |
| OpenAI-only beta | Change: beta provider selection is explicit; native OpenAI and Anthropic launch first, exact custom profiles are conformance-gated, and built-in OpenRouter/Z.AI remain gated |
| Fixed Provider-call count | Replace with admitted semantics: one direct call or `N + 2` team calls; discovery/conformance is explicit outside the Run; one provider/model/route is pinned for every admitted call |
| Strict Structured Outputs | Preserve: response JSON Schema or documented strict forced tool only; JSON mode/local repair is insufficient |
| `ARANY_OPENAI_MODEL` | Preserve for OpenAI; add provider-specific model variables and `ARANY_PROVIDER`; `--model` applies only to the selected provider |
| Fixed OpenAI origin | Generalize to native compiled origins plus trusted exact custom profiles; no arbitrary per-request URL, redirects, ambient proxy, or header injection |
| No retries | Preserve, including disabling OpenRouter upstream fallbacks |
| One Provider trait | Preserve; adapters deepen the existing seam, while shared HTTP code remains a private concrete implementation detail |

## Canonical documents that must change

This report changes no existing decision by itself. Before implementation, update these authoritative documents together so they do not disagree:

| Document | Required change |
| --- | --- |
| `AGENTS.md` | Replace “scripted fake and OpenAI are real adapters” with the staged adapter set while preserving Provider as the only Engine behavior seam |
| `docs/architecture/system-overview.md` | Update the physical component map, provider construction/preflight, configuration, canonical provenance, fixed-origin matrix, and rollout order |
| `docs/architecture/arany-conceptual-overview.md` | Update the beta backend table, diagrams, file sketch, risks, and milestone/gate descriptions |
| `docs/research/next-step-decision-register.md` | Supersede the OpenAI-only V1 provider/config decisions with explicit staged decisions and gates |
| `docs/research/cli-inputs-configuration-and-operability.md` | Define `--provider`, provider-specific models/keys, diagnostics, endpoint profiles, OpenRouter route controls, and help text |
| `agents/security.md` | Generalize fixed Provider egress to a compiled provider/region origin allowlist and add broker/upstream privacy and provenance invariants |
| `agents/testing.md` | Replace the single OpenAI-only live-smoke rule with the explicit per-adapter, opt-in conformance matrix while retaining one scripted Engine double |
| `docs/research/rust-foundation-and-engine-contract.md` | Amend the concrete adapter, config, provenance, error, and direct-HTTP sections without changing the Engine workflow |
| `docs/research/testing-strategy-for-rust-cli-harness.md` | Amend fixture ownership and the ignored live-test matrix |
| `docs/research/provider-and-tool-runtime.md` | Supersede its OpenAI-only scope amendment and link this narrower decision; retain its general warning against universal wire formats |
| `docs/research/research-closure-audit.md` | Update provider parity and rollout gates so the closure claim reflects the strict Z.AI gap and constrained OpenRouter route |
| `docs/research/README.md` | Index this report and replace the current “fake and OpenAI” decision summary |

Older exploratory research that merely records investigated alternatives should not be rewritten as current architecture. Any file that presents the superseded OpenAI-only choice as a current decision, however, needs an explicit dated amendment or a link to the updated canonical decision rather than silent historical residue.
