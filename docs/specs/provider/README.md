# Provider

## Scope and related contracts

This contract owns Provider selection and pinning, native and custom admission, outcome and Tool encoding, egress, usage provenance and provisional display. Accepted outcomes and deadlines belong to the [execution contract](../execution/events-and-outcomes.md#accepted-outcomes), saved-account storage to the [credential contract](../credentials.md), ChatGPT-plan OAuth and its output bound to the [ChatGPT contract](../chatgpt.md), and plan contracts to the [goals contract](../goals.md#initial-plan-contract-and-success-receipt). Live support per route needs the owner-run checks in [next steps](../../../NEXT_STEPS.md#beta-2-and-broader-release); offline fixtures establish neither live support nor an account-level zero-data-retention claim. Editing constraints: [security rules](../../../agents/security.md).

- Each role has an explicitly selected profile, model, reasoning profile, privacy profile, outcome encoding and call limits, pinned with every configured fallback before Workspace input and `RunStarted`. A Session changes its defaults only between Runs. Unconfigured roles inherit the primary route ([role routing](./selection.md#per-role-selection-and-fallback)).
- Provider is the substitutable semantic inference interface. Requests and responses carry Arany semantics, never wire objects or a generic parameter bag.
- `ProviderKind` owns native Provider names and the `custom:` profile syntax for runtime identities and CLI route construction. Native credential classification and catalog endpoint, header and paging selection use it, parsing exact public profile strings at the boundary. Custom and ChatGPT profiles cannot select API-key native discovery.
- A primary Goal call may carry a bounded typed `GoalContextFacts` projection. It is context, not a grant or budget override; children never receive it.
- `Provider::compact` is a separate semantic call, not a synthetic AgentRun. It receives bounded accepted outcomes and an optional prior summary, never Workspace instructions or includes. API-key and custom routes enforce the output cap remotely; the ChatGPT-plan route records a local-only cap. The summary is untrusted data, portable only with its provenance.
- Rollout is staged: native OpenAI and Anthropic first, then exact custom profiles; OpenRouter is deferred behind its route and privacy gate.
- Selection is deterministic from admitted user configuration. There is no model-selected destination, hidden discovery call or preliminary model check inside a Run. Fallback is off unless explicitly configured.

Topic files:

- [Support, selection and presets](./selection.md): Provider support summary; Per-role selection and fallback; Named roster presets; Native models and accounts; Commands.
- [Outcome encoding, caching and streaming](./encoding-and-streaming.md): Structured outcomes and Tool encoding; Prompt caching; Streaming and provisional text.
- [Configuration, routing and usage](./routing-and-usage.md): Configuration and egress; Custom destinations and routing; Usage, provenance and reliability.
