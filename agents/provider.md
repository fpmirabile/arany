# Provider

Load when changing Provider selection, model configuration, endpoint profiles, credentials, request/response mapping, structured outcomes, routing, usage, privacy provenance, or provider conformance.

The capability/routing evidence lives in `docs/research/beta-multi-provider-routing-and-adapters.md`; the reason consumer-subscription authentication is deferred lives in `docs/research/consumer-subscription-authentication-for-provider-adapters.md`. Keep this file as the working Provider contract.

## Beta contract

- One explicitly selected ProviderProfile, model, reasoning profile, privacy profile, and outcome encoding is pinned before Workspace input and `RunStarted`; every call in that Run uses the resolution. A Session may change its default only between Runs.
- `Provider` remains the only Engine behavior seam. Engine requests and responses contain Arany semantics, never provider wire objects or a generic parameter bag.
- Product rollout is staged: native OpenAI and Anthropic first; exact custom endpoint/model profiles join only after data-free conformance; a built-in constrained OpenRouter profile remains deferred behind its route/privacy gate; Z.AI remains unadmitted until an exact profile passes a documented strict outcome contract. The scripted Provider is test-only.
- Provider switching is deterministic selection, not automatic routing. There is no cross-provider/model fallback or hidden discovery call inside a Run.

## Strict outcome rule

- Prefer provider-enforced JSON Schema for `Delegate | Finish`.
- A synthetic `submit_agent_outcome` call is an allowed private encoding only when the exact provider/model officially supports strict tool arguments and forced tool selection. It is decoded and validated, never executed.
- JSON mode, prompt-only formatting, automatic tool selection, local repair, or schema validation after unconstrained generation cannot satisfy the beta contract.
- Select the encoding from reviewed capability evidence before `RunStarted`. Refusal, truncation, duplicate/missing call, or schema mismatch fails that Provider call without retrying another encoding.

## Configuration and egress

- Resolve an explicit CLI profile/model over trusted Session or user-config defaults. There is no compiled model default, Workspace-defined profile, or implicit discovery.
- Read only the selected provider's credential. Credentials never enter CLI arguments, Events, output, fixtures, telemetry, or error bodies.
- Beta adapters use their explicit API-key sources only. Do not implement dormant OpenAI plan OAuth while its route lacks Arany's remote output cap; never import another harness's tokens or use a private ChatGPT route. Anthropic remains API-key only unless it grants Arany prior approval.
- Native endpoints come from compiled reviewed profiles. A custom profile lives in trusted user configuration outside the Workspace and names a closed protocol family, exact normalized endpoint/base path, exact model, credential reference, outcome encoding, and capability-evidence version. It never accepts arbitrary headers, shell credential commands, redirects, ambient proxies, or cross-profile credential reuse.
- Before a custom profile receives Workspace data, `arany provider check PROFILE` runs a bounded synthetic conformance sequence and records expiring evidence keyed by Arany/adapter/test versions, exact origin, model, encoding, and output cap. A catalog or model list is not evidence. Unverified profiles are usable only by `provider check`.
- Require HTTPS outside explicit numeric loopback. Pin destination and reject redirect, route/model drift, changed addresses, metadata/link-local/multicast destinations, and credential reflection. Receipts say `custom verified`; they never relabel a service as native OpenAI/Anthropic.
- A future built-in OpenRouter profile treats it as a broker: pin one reviewed upstream endpoint; require parameter support; disable fallback; deny data collection; require ZDR; disable response caching; and record broker plus upstream provenance.
- Similar HTTP mechanics may use private concrete helpers. Each adapter owns authentication, request documents, strict encoding, error mapping, usage, privacy, and response parsing. Do not create a transport trait or a universal OpenAI-compatible adapter.

## Reliability and evidence

- Keep zero automatic retries. Provider calls, tokens, bytes, time, concurrency, and admitted children share one immutable aggregate Run budget sized from the resolved collaboration policy. A lost response may already have consumed tokens; broker fallback is also a retry and stays disabled.
- Persist bounded adapter, endpoint profile, requested/resolved model, request ID, broker/upstream identity, finish reason, usage provenance, privacy controls, and capability-evidence version. Never persist raw response bodies or uncontrolled headers.
- The scripted Provider owns Engine tests. Each product adapter owns hand-reviewed offline wire fixtures and ignored, explicitly activated, credential-and-model-gated live direct/team Runs.
- A catalog lists availability, not compatibility. Only reviewed manifests, adapter fixtures, and opt-in live probes support capability claims.
