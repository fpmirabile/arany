# Provider

## Scope and related contracts

This contract owns Provider selection and pinning, native and custom admission, outcome and Tool encoding, egress, usage provenance and provisional display. Accepted outcomes and deadlines belong to the [execution contract](../execution/events-and-outcomes.md#accepted-outcomes), saved-account storage to the [credential contract](../credentials.md), ChatGPT-plan OAuth and its output bound to the [ChatGPT contract](../chatgpt.md), and plan contracts to the [goals contract](../goals.md#initial-plan-contract-and-success-receipt). Live support per route needs the owner-run checks in [next steps](../../../NEXT_STEPS.md#beta-2-and-broader-release); offline fixtures prove neither live support nor account-level zero data retention. Editing constraints: [security rules](../../../agents/security.md).

- Each role pins an explicitly selected profile, model, effort, privacy profile, outcome encoding and call limits, with every configured fallback, before Workspace input and `RunStarted`; unconfigured roles inherit the primary route ([role routing](./selection.md#per-role-selection-and-fallback)). A Session changes its defaults only between Runs.
- Selection is deterministic from admitted user configuration and exact profile strings (native names or `custom:` profiles): no model-selected destination, hidden discovery call or in-Run model check, and no fallback unless configured. Custom and ChatGPT profiles cannot select API-key native discovery.
- Requests carry Arany semantics, never wire objects or a generic parameter bag. A primary Goal call may carry bounded typed Goal facts: context, never a grant or budget override; children never receive them.
- Compaction is a separate call, not an AgentRun. It receives bounded accepted outcomes and an optional prior summary, never Workspace instructions or includes. API-key and custom routes enforce its output cap remotely; the ChatGPT-plan route caps locally. The summary is untrusted data, portable only with its provenance.
- Rollout: native OpenAI and Anthropic, then exact custom profiles; OpenRouter is deferred behind its route and privacy gate.

Topic files:

- [Support, selection and presets](./selection.md): Provider support summary; Per-role selection and fallback; Named roster presets; Native models and accounts; Commands.
- [Outcome encoding, caching and streaming](./encoding-and-streaming.md): Structured outcomes and Tool encoding; Prompt caching; Streaming and provisional text.
- [Configuration, routing and usage](./routing-and-usage.md): Configuration and egress; Custom destinations and routing; Usage, provenance and reliability.
