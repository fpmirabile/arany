# Provider

Load when changing Provider selection, model configuration, endpoint profiles, credentials, request/response mapping, structured outcomes, routing, usage, privacy provenance, or provider conformance.

The capability/routing evidence lives in `docs/research/beta-multi-provider-routing-and-adapters.md`; the current ChatGPT-plan decision and its weaker output-bound trade-off live in `docs/research/next-step-decision-register.md` (D-18). This file retains account/profile admission and adapter editing constraints; the [execution spec](../docs/specs/execution.md#accepted-outcomes) owns accepted Run outcomes and their scenarios.

## Load for the affected task

- Native model/effort selection, credentials, catalogs or checks → [native models and accounts](./references/provider-details.md#native-models-and-accounts).
- Responses decoding, coding requests or Tool catalog projection → [structured outcomes and Tool encoding](./references/provider-details.md#structured-outcomes-and-tool-encoding).
- Custom profiles, destination admission, DNS or broker routing → [custom destinations and routing](./references/provider-details.md#custom-destinations-and-routing).
- ChatGPT discovery, conformance or wire behavior → [ChatGPT catalogs, checks and streaming](./references/provider-details.md#chatgpt-catalogs-checks-and-streaming) and [account rules](./chatgpt.md).

## Beta contract

- One explicitly selected ProviderProfile, model, reasoning profile, privacy profile, and outcome encoding is pinned before Workspace input and `RunStarted`; every call in that Run uses the resolution. A Session may change its default only between Runs.
- Pin the Provider-declared concurrency ceiling with the selected profile before Workspace input; `RunStarted` records it. Legacy `RunStarted` payloads without the field replay conservatively with a ceiling of one.
- `Provider` remains the only Engine behavior seam. Engine requests and responses contain Arany semantics, never provider wire objects or a generic parameter bag.
- `Provider::compact` is a separate semantic call, not a synthetic AgentRun. It receives bounded accepted conversation outcomes (including unanswered objectives) and optional prior derived summary, and excludes Workspace instructions/includes. API-key and custom Providers enforce the requested output cap remotely; the consented ChatGPT-plan route records a local-only acceptance cap because its published request rejects a remote cap. The resulting summary is untrusted data, portable across Providers only with its recorded provenance.
- Product rollout is staged: native OpenAI and Anthropic first; exact custom endpoint/model profiles join only after data-free conformance; a built-in constrained OpenRouter profile remains deferred behind its route/privacy gate; Z.AI remains unadmitted until an exact profile passes a documented strict outcome contract. The scripted Provider is test-only.
- Provider switching is deterministic selection, not automatic routing. There is no cross-provider/model fallback or hidden discovery call inside a Run.

## Strict outcome rule

- Prefer provider-enforced JSON Schema for `Delegate | Finish`, restricted by typed phase and collaboration policy. Read-only native/custom calls and Anthropic coding calls use structured text. Tool-enabled OpenAI/ChatGPT primary calls use strict native function definitions for catalog-available Tool operations and phase-available Finish/Delegate submissions, `tool_choice: required` and disabled parallel calls. Exactly one completed function call translates to the same Engine semantic outcome; reject final text, multiple/unknown/incomplete calls and invalid typed arguments, without encoding fallback. Children reject Tool context before transport. Preserve complete-response/model/usage and credential-reflection checks. Exact custom profiles remain read-only and reject Tool context before HTTP; future support needs exact tool-capable evidence.
- A synthetic `submit_agent_outcome` call is an allowed private encoding only when the exact provider/model officially supports strict tool arguments and forced tool selection. It is decoded and validated, never executed.
- JSON mode, prompt-only formatting, unconstrained automatic tool selection, local repair, or schema validation after unconstrained generation cannot satisfy the beta contract.
- Select the encoding from reviewed capability evidence before `RunStarted`. Refusal, truncation, duplicate/missing call, or schema mismatch fails that Provider call without retrying another encoding.

## Configuration and egress

- Resolve an explicit CLI profile/model over trusted Session or user-config defaults. There is no compiled model default, Workspace-defined profile, or implicit discovery.
- Read only the selected provider's credential. Credentials never enter CLI arguments, Events, output, fixtures, telemetry, or error bodies.
- Every Provider HTTP client requests identity encoding, disables automatic decompression even if another dependency enables reqwest compression features, rejects any non-identity `Content-Encoding` value before reading the body, and bounds both declared length and accumulated chunks to 1 MiB per response. Compressed Provider replies are deliberately unsupported.
- Require HTTPS outside explicit numeric loopback. Pin destination and reject redirect, route/model drift, changed addresses, metadata/link-local/multicast destinations, and credential reflection. Receipts say `custom verified`; they never relabel a service as native OpenAI/Anthropic.
- Use Reqwest's published platform TLS verifier with certificate-chain and server-name checks intact. Arany's endpoint and payload admission does not control auxiliary certificate/revocation connections made by the operating system during trust evaluation; never treat them as Provider destinations for content, files, or credentials.
- Similar HTTP mechanics may use private concrete helpers. Each adapter owns authentication, request documents, strict encoding, error mapping, usage, privacy, and response parsing. Do not create a transport trait or a universal OpenAI-compatible adapter.

## Reliability and evidence

- Keep zero automatic retries. Provider calls, tokens, bytes, time, concurrency, and admitted children share one immutable aggregate Run budget sized from the resolved collaboration policy. Each public adapter rejects reported input above the shared ceiling and output above the exact request cap before returning success; Engine admission remains defense in depth, not the adapter's sole bound. Native OpenAI retains optional unknown usage; custom transport retains required positive usage. A lost response may already have consumed tokens; broker fallback is also a retry and stays disabled.
- Each observed Run call commits one bounded `ProviderCallRecorded` fact before its AgentRun terminal Event. Its phase and local disposition are canonical; response ID and provider-reported input/output usage are present only when a bounded response was received. A timed-out or cancelled call reports unknown usage and does not prove absence of remote billing. `RunStarted` remains the selected Provider/model/effort provenance. Accepted wire provenance distinguishes Responses `completed` with a local `store: false` request from Messages `end_turn` with no equivalent request switch; it is not a remote retention or account-level ZDR guarantee. Compaction records carry the same optional provenance.
- API-key adapters reject exact selected-key reflection in bounded raw response bytes and decoded outcome, response-ID, or compaction fields before any result can become a canonical Event. This includes JSON-escaped echoes but is not general secret detection.
- MCP argument strings are a second JSON boundary: Provider Tool admission and credential scanning use the same bounded duplicate-free object decoder as the Guard. Reject malformed/non-object arguments before persistence; a last-key-wins parse or failed parse cannot hide an escaped credential.
- Persist bounded adapter, endpoint profile, requested/resolved model, request ID, broker/upstream identity, finish reason, usage provenance, privacy controls, and capability-evidence version. Never persist raw response bodies or uncontrolled headers.
- The scripted Provider owns Engine tests. Each product adapter owns hand-reviewed offline wire fixtures and ignored, explicitly activated, credential-and-model-gated live Runs. Native API-key adapters test direct and team Runs separately; the ChatGPT-plan gate first performs its account-bound synthetic model/effort check, then one direct Run with exact JSONL/Store replay and local-only bound provenance. No ignored test is release evidence until it actually passes against the selected account.
- A catalog lists availability, not compatibility. Only reviewed manifests, adapter fixtures, and opt-in live probes support capability claims.
