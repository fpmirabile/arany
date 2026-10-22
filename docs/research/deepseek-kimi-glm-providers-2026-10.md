# DeepSeek, Kimi and GLM through Anthropic-compatible routes

Source-check date: **2026-10-21**; GLM recheck: **2026-10-22**. Question: can Arany use these vendors with
API keys and, where authorized, consumer coding subscriptions without adding
another wire adapter or impersonating a vendor client?

Conclusion: Anthropic-compatible custom routes reuse the existing adapter.
DeepSeek API, Kimi platform/Code and GLM Standard are eligible for exact-profile
text-compaction conformance; live compatibility remains unverified.
GLM Coding Plan is excluded by its supported-tool authorization boundary.
The [Provider contract](../specs/provider.md#provider-support-summary) owns route
admission; [setup](../setup.md#anthropic-compatible-custom-routes) owns recipes.

Scope: official documentation and pinned peer source review. No credentials,
paid requests, live `provider check`, native-platform checks or conformance
results were obtained. **Unverified** means an open evidence gap, not permission
to approximate strict outcomes. Recheck models, authorization and billing
before implementation or paid use.

## Endpoint and authorization evidence

| Route | Anthropic base URL | Key and model evidence |
|---|---|---|
| DeepSeek API | `https://api.deepseek.com/anthropic` | DeepSeek API key; `deepseek-flash`, `deepseek-v4-pro` ([compatibility guide](https://api-docs.deepseek.com/guides/anthropic_api/), [model details](https://api-docs.deepseek.com/quick_start/pricing/)) |
| Kimi platform API | `https://api.moonshot.ai/anthropic` | Platform API key; Messages reference explicitly lists `kimi-k3` ([Messages API](https://platform.kimi.ai/docs/api/messages)) |
| Kimi Code subscription | `https://api.kimi.com/coding/` | Kimi Code Console key; `k3`, `k3-256k`, `kimi-for-coding`, `kimi-for-coding-highspeed`, subject to membership tier ([model configuration](https://www.kimi.com/code/docs/en/kimi-code/models.html)) |
| GLM standard API | `https://api.z.ai/api/anthropic` | Newly created **Standard API Key**; current model candidates `glm-5.3`, `glm-5.3-flash` ([key management](https://docs.z.ai/devpack/notice/api-key-management.md), [model integration](https://docs.z.ai/devpack/tool/claude)) |

These are base URLs, not interchangeable full request URLs. Kimi's platform
reference explicitly gives `POST https://api.moonshot.ai/anthropic/v1/messages`.
Other full paths must be confirmed by the implementation's endpoint fixtures
and owner checks; no cross-vendor discovery or fallback is implied.

### DeepSeek: API key only

The compatibility guide demonstrates the Anthropic SDK with `deepseek-flash`.
Its `claude*` aliases apply to otherwise unsupported model names; it does
**not** require every request model to begin with `claude`. Use the real
DeepSeek IDs, avoiding alias-dependent model identity.

The documented compatibility deltas are concrete: `x-api-key` is supported;
`thinking` is accepted but `budget_tokens` ignored; `output_config` supports
**only `effort`**; tool schemas and `none`/`auto`/`any`/named-tool choices are
supported, but `disable_parallel_tool_use` is ignored. Thinking blocks are
supported; `redacted_thinking`, document and MCP blocks are unsupported;
`cache_control` markers are ignored.
([Anthropic compatibility](https://api-docs.deepseek.com/guides/anthropic_api/))

The pricing page identifies `deepseek-flash` as V4.1-Flash-1017 and
`deepseek-v4-pro` as V4-Pro-0813, with token billing against account balance.
No subscription integration was established by the reviewed material; this
plan therefore includes only pay-as-you-go API access.
([Models and pricing](https://api-docs.deepseek.com/quick_start/pricing/))

**Admission implication:** `output_config.format` is outside the documented
supported fields. Arany's [plain-text compaction contract](../specs/provider.md#structured-outcomes-and-tool-encoding)
does not request it. The exact route must still pass all text, delegation,
streaming, identity and usage checks before Workspace disclosure.

Account concurrency exhaustion returns 429. The reviewed rate-limit and error
pages specify no `Retry-After` contract; its presence or absence on live
responses remains **unverified**. Authentication failure is 401 and depleted
balance 402. Test bounded handling when retry guidance is absent.
([Rate limits](https://api-docs.deepseek.com/quick_start/rate_limit/),
[errors](https://api-docs.deepseek.com/quick_start/error_codes/))

### Kimi: platform API and Code subscription

The platform Messages reference specifies Bearer authorization, streaming,
thinking/text/tool-use blocks and `output_config` for effort and structured
output. `input_tokens` excludes cache reads and cache writes; total input adds
`cache_read_input_tokens` and `cache_creation_input_tokens`. Its example also
includes `output_tokens_details.thinking_tokens`. Preserve these distinctions
when testing usage accounting; optional thinking-token details must not be
added to the output total again. The same reference and OpenAPI document
`cache_creation` with required `ephemeral_5m_input_tokens` and
`ephemeral_1h_input_tokens`. Anthropic documents their sum as equal to the flat
creation total; Arany's [usage contract](../specs/provider.md#usage-provenance-and-reliability)
owns normalization and disagreement rejection.
([Messages API](https://platform.kimi.ai/docs/api/messages),
[Anthropic cache usage](https://platform.claude.com/docs/en/build-with-claude/prompt-caching#mixing-different-ttls))

Checked 2026-10-21: the official OpenAPI `MessagesRequest.output_config.format`
accepts `{ "type": "json_schema", "schema": … }` and promises schema-constrained
JSON. No additional `strict` flag is documented there. Schema support is not
required by Arany's text-compaction contract; exact conformance remains required.
([Official OpenAPI definition](https://platform.kimi.ai/docs/openapi.json))

The platform overview also names `kimi-k2.7-code-highspeed` and `kimi-k2.6`,
but the reviewed Messages reference lists only `kimi-k3`. Messages support for
those additional platform models is **unverified**; do not infer it from Chat
Completions examples.
([Platform quickstart](https://platform.kimi.ai/docs/overview))

Kimi Code is a distinct subscription service with console-created keys,
separate from pay-as-you-go platform keys. Its membership guide explicitly
permits benefits in third-party development tools and requires the client's
real identity; falsifying its User-Agent can suspend benefits. The owner
accepts this as sufficient authorization to pursue Arany integration with an
honest `arany/<version>` User-Agent. Do not impersonate Kimi CLI or reuse its
OAuth client ID. No Arany OAuth registration was established or is proposed.
([Membership guide](https://www.kimi.com/en/help/kimi-code/membership-guide))

The Code model guide documents China `.com` and overseas `.ai` endpoints.
The selected recipe uses `https://api.kimi.com/coding/`; the documented overseas
equivalent is `https://api.kimi.ai/coding/`. Selection must be explicit, with
its own credential and conformance scope, never an automatic redirect or retry
destination. The guide maps reasoning efforts to `low`/`high`/`max`; unsupported
values can return 400. Entitlement failures can return 401 even with a valid
key, and a mistyped HighSpeed ID can silently select the standard model.
Test returned identity and entitlement diagnosis instead of treating every
401 as a bad secret.
([Model configuration](https://www.kimi.com/code/docs/en/kimi-code/models.html))

The Code Claude integration documents `ANTHROPIC_API_KEY` authentication and
explains that `k3[1m]` is Claude Code syntax: other API clients use `k3`.
Keep this client-specific suffix out of Arany model IDs.
([Code integration](https://www.kimi.com/code/docs/en/third-party-tools/claude-code))

**Code admission:** the reviewed documents establish console keys and Messages
compatibility, not successful Arany use. The endpoint needs its own exact
text-compaction check; platform evidence cannot be transferred to Code.
[NEXT_STEPS](../../NEXT_STEPS.md#daily-use-first) owns live/platform checks.

### GLM: Standard API Key; subscription excluded

The October 16, 2026 key-management update separates Standard API Keys from
Coding Plan keys. Newly created Standard keys use account balance or standard
bundles across supported endpoints. Its table names
`https://api.z.ai/api/anthropic` as the general Anthropic Messages endpoint;
the migration FAQ permits replacing an old key with a Standard key while
keeping the endpoint. This establishes a documented API-key recipe candidate.
Pre-update **Compatible** keys can still consume Coding Plan quota at that
URL when the account has held a plan, so they are unsuitable for this recipe.
Require an explicitly classified Standard key and owner confirmation of
standard billing.
([API key management, published 2026-10-16](https://docs.z.ai/devpack/notice/api-key-management.md))

The current Claude integration names GLM-5.3 and GLM-5.3-Flash and configures
`ANTHROPIC_AUTH_TOKEN` with the Anthropic base URL. These are model candidates,
not proof that Arany's complete native wire contract works.
([Claude integration](https://docs.z.ai/devpack/tool/claude))

GLM-5.3 uses forced reasoning with `low`/`high`/`max` effort. The thinking
guide requires preservation of reasoning across tool rounds, but its concrete
`reasoning_content` examples use Chat Completions. Exact mapping to Anthropic
thinking blocks, signatures, text compaction and usage is **unverified**.
Test the Messages wire itself; do not paste Chat-only fields into it.
([GLM-5.3](https://docs.z.ai/guides/llm/glm-5.3),
[thinking guide](https://docs.z.ai/guides/capabilities/thinking-mode))

Checked 2026-10-22: the structured-output guide uses Chat Completions
`response_format: { "type": "json_object" }`, prompt-defined structure and
client-side schema validation. This does not establish provider-enforced
Messages schemas, which Arany's text-compaction contract does not require.
The Standard API candidates still need exact checks and live evidence.
([Structured-output guide](https://docs.z.ai/guides/capabilities/struct-output))

The GLM Coding Plan tool page confines subscription use to its listed tools;
Arany is absent. Section 4 of the subscription terms limits quota to supported
tools and excludes general API integrations without a separate written
agreement. **Exclude the subscription**, regardless of endpoint compatibility.
Revisit only if Z.ai adds or explicitly approves Arany. Do not use a ZCode
OAuth identity or another tool's User-Agent to bypass this boundary.
([Supported tools](https://docs.z.ai/devpack/tool/others),
[subscription terms, section 4](https://docs.z.ai/legal-agreement/subscription-terms))

## Peer precedent and rejected alternatives

Pi's Anthropic Messages implementation accepts the model's explicit `baseUrl`
and the resolved API key when constructing its Anthropic client. Its custom
model documentation supports changing an Anthropic base URL. This is a source
precedent for one shared adapter with a configurable destination; it does not
establish Arany conformance or authorize sharing credentials across routes.
Source reviewed at commit `cae9fd28a2d3d515c83ea495093e3768cc8efab6`:
[client construction, lines 1104–1108](https://github.com/earendil-works/pi/blob/cae9fd28a2d3d515c83ea495093e3768cc8efab6/packages/ai/src/api/anthropic-messages.ts#L1104-L1108),
[base-URL customization](https://github.com/earendil-works/pi/blob/cae9fd28a2d3d515c83ea495093e3768cc8efab6/packages/coding-agent/docs/models.md#L69-L76).

One shared Anthropic-compatible adapter avoids another vendor enum or wire
protocol. A private or undocumented first-party login for another provider is not,
by itself, a third-party authorization grant for Arany.

## Admission implications

The [Provider contract](../specs/provider.md#custom-destinations-and-routing)
owns exact endpoint/model checks, text compaction, usage, cancellation, bounds,
key isolation and truthful client identity. The [setup recipes](../setup.md#anthropic-compatible-custom-routes)
follow explicit Messages base/key selection, matching Kimi's
[Claude Code](https://www.kimi.com/code/docs/en/third-party-tools/claude-code) and
[OpenCode](https://www.kimi.com/code/docs/en/third-party-tools/opencode) integration
recipes. Another client's successful chat cannot establish Arany's durable
provenance or pre-disclosure text-compaction admission.

Unresolved vendor, billing and native evidence and the owner-run procedures live
only in [NEXT_STEPS](../../NEXT_STEPS.md#daily-use-first). No live success,
platform parity or subscription billing result is claimed.
