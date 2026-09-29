# Consumer subscription authentication for provider adapters

Retrieved: 2026-09-29

Status: research recommendation, not an implemented or approved product contract

Scope: whether Arany may let a local user consume OpenAI/Codex or Anthropic Claude through a consumer subscription instead of an API key; how current harnesses implement that experience; and the smallest compliant beta design. This report distinguishes facts in official documentation or pinned upstream source from inference and recommendation. Product terms and preview programs can change, so the implementation must re-check the cited sources before release.

## Executive decision

OpenAI and Anthropic must not be treated as equivalent merely because both first-party CLIs accept consumer subscriptions.

| Product path | What pays for inference | Arany beta decision | Reason |
| --- | --- | --- | --- |
| OpenAI API key | OpenAI API project billing | Support | Stable public provider path |
| OpenAI official ChatGPT-plan sharing for open-source local apps | An eligible signed-in ChatGPT plan | Do not ship inference under the current beta contract | Authentication is official, but the route requires streaming and rejects the server-side output-token cap required by Arany's aggregate Run budget |
| Reusing Codex/OpenCode OAuth credentials or private ChatGPT routes | A ChatGPT plan through copied first-party or legacy behavior | Prohibit | It bypasses the official third-party flow and creates credential, endpoint, and policy risk |
| Anthropic API key | Anthropic Console/API billing | Support | Anthropic's documented third-party product path |
| Direct “Sign in with Claude” for Pro/Max | A Claude consumer subscription | Prohibit | Anthropic explicitly reserves subscription OAuth for Claude Code/native applications unless it grants prior approval |
| Unmodified Claude Code binary or Claude Agent SDK delegation | A separately authenticated Claude product under Anthropic's rules | Defer | This imports another agent loop and requires a distinct product, legal, security, and architecture decision |

**Revised recommendation after the dependency and budget review:** beta should ship API-key authentication for OpenAI and Anthropic. The official OpenAI `chatgpt` authentication design is feasible and should remain the sole candidate subscription design, but plan-funded inference is release-blocked while Arany requires a provider-enforced 4,096-output-token ceiling on every call. Do not add dormant OAuth, listener, browser, or keyring dependencies merely to ship login without a usable Provider profile. When the inference gate reopens, `api-key` and `chatgpt` are explicit credential modes under the same semantic OpenAI Provider adapter. Anthropic remains `api-key` only. Neither adapter may import another CLI's tokens, impersonate its OAuth client, or silently change billing modes.

The official OpenAI path has two independent gates. It supports a constrained form of the public Responses API, so Arany must prove that the exact account, model, and request encoding enforce the fixed strict `Delegate | Finish` outcome. More fundamentally, the route currently rejects `max_output_tokens`; therefore it cannot satisfy the existing fixed token-budget invariant even if strict output passes. If either gate fails, Arany must reject the profile before `RunStarted`; it must not weaken the contract or quietly charge an API key.

## The subscriptions are not API credits

### Anthropic

**Fact:** Claude Pro, Max, Team, and Enterprise subscriptions do not include Anthropic Console/API access or API credits. Console billing is separate. Pro and Max can include Claude Code, but Claude Code is Anthropic's own product and its subscription authentication is not a general-purpose API credential. ([Anthropic: subscription and API billing are separate](https://support.claude.com/en/articles/9876003-i-have-a-paid-claude-subscription-pro-max-team-or-enterprise-plans-why-do-i-have-to-pay-separately-to-use-the-claude-api-and-console), [Claude Code with Pro or Max](https://support.claude.com/en/articles/11145838-use-claude-code-with-your-pro-or-max-plan))

**Fact:** Anthropic's API documents API keys, workload identity federation, and App Attest as authentication mechanisms. It does not document a consumer-subscription OAuth grant for arbitrary provider clients. ([Anthropic API authentication](https://platform.claude.com/docs/en/manage-claude/authentication))

### OpenAI

**Fact:** ChatGPT and the OpenAI API normally have separate billing systems. A ChatGPT subscription does not generally fund API usage. ([OpenAI: ChatGPT and API billing](https://help.openai.com/en/articles/9039756-managing-billing-for-chatgpt-and-the-api-platform))

**Fact:** “Codex subscription” is useful shorthand, but Codex is currently an entitlement included with eligible ChatGPT plans rather than a portable credential or a separate OpenAI API balance. Plan availability and usage limits vary, so Arany must rely on the scope and model catalog actually granted to the signed-in account rather than infer entitlement from a plan label. ([OpenAI: using Codex with a ChatGPT plan](https://help.openai.com/en/articles/11369540-using-codex-with-your-chatgpt-plan))

**Fact:** OpenAI now documents an explicit exception: eligible open-source, locally hosted applications may let a user invoke a constrained subset of the public Responses API using a ChatGPT plan through the “Sign in with ChatGPT” open-source flow. That grant exposes neither ChatGPT conversation history nor arbitrary ChatGPT account data. Paid or remotely hosted products must contact OpenAI rather than assuming the open-source/local eligibility applies. ([OpenAI open-source token sharing overview](https://developers.openai.com/siwc/token-sharing-open-source))

**Inference:** “Codex is included in my ChatGPT plan” and “my API account has credit” remain different claims. Arany can use a ChatGPT plan only through the specifically granted resource, scope, endpoint, and capability subset. It cannot reinterpret a general ChatGPT session as an API bearer token.

## What OpenCode currently does

The source observations in this section are pinned to OpenCode commit [`7945de2`](https://github.com/anomalyco/opencode/commit/7945de208964a49300d7f770d1a71d078db9a4c4). They explain OpenCode's behavior; they are not authorization for Arany to copy it.

### OpenAI/ChatGPT browser flow

**Fact:** OpenCode's built-in Codex plugin uses the static Codex OAuth client ID `app_EMoamEEZ73f0CkXaXp7hrann`. It opens `https://auth.openai.com/oauth/authorize`, requests `openid profile email offline_access`, uses PKCE S256 and `state`, and receives the callback at `http://localhost:1455/auth/callback`. It sends Codex-specific flags and `originator=opencode`. Token exchange and refresh use `https://auth.openai.com/oauth/token`. ([OpenCode Codex adapter](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/plugin/openai/codex.ts))

**Fact:** the adapter rewrites inference requests to `https://chatgpt.com/backend-api/codex/responses` and sends both the bearer token and a `ChatGPT-Account-Id` header. It exposes only a selected model subset for OAuth accounts. The adapter is bundled into OpenCode rather than being only a user-installed plugin. ([OpenCode Codex adapter](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/plugin/openai/codex.ts), [OpenCode built-in plugin registration](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/plugin/index.ts))

### OpenAI/ChatGPT device flow

**Fact:** in headless mode, OpenCode starts at `POST https://auth.openai.com/api/accounts/deviceauth/usercode`, asks the user to visit `https://auth.openai.com/codex/device`, polls `POST https://auth.openai.com/api/accounts/deviceauth/token`, and exchanges the resulting authorization code and verifier at the ordinary OAuth token endpoint using `https://auth.openai.com/deviceauth/callback`. ([OpenCode Codex adapter](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/plugin/openai/codex.ts))

**Fact:** refresh is single-flighted within one OpenCode process. The browser flow has a five-minute callback timeout; the device polling loop has no visible overall deadline in the inspected source. The browser flow checks `state` and uses PKCE, but it does not send an OIDC `nonce`; token claims are decoded for account metadata without visible signature validation in this adapter. Its listener calls `listen(port)` and uses `localhost` rather than explicitly binding the address required by OpenAI's newer third-party flow. ([OpenCode Codex adapter](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/plugin/openai/codex.ts))

### OpenCode credential persistence

**Fact:** OpenCode stores provider credentials in `Global.Path.data/auth.json`, documented on Linux as `~/.local/share/opencode/auth.json`. OAuth entries contain plaintext access and refresh tokens, expiry, and optional account/provider metadata. The file is written with Unix mode `0600`. The inspected storage module shows no OS keyring, encryption, explicit cross-process locking, or explicit atomic replacement. `OPENCODE_AUTH_CONTENT` can supply the whole credential map in an environment variable. ([OpenCode authentication storage](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/auth/index.ts), [OpenCode provider documentation](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/web/src/content/docs/providers.mdx#L18-L22))

### Claude support was removed

**Fact:** OpenCode removed its bundled `opencode-anthropic-auth` plugin, the “Claude Max or API key” option, and Claude Code-specific prompt/header behavior in the 2026-02-19 commit titled “anthropic legal requests.” Current provider documentation says Anthropic prohibits this subscription-auth mechanism and any remaining community plugins are not bundled. ([OpenCode removal commit](https://github.com/anomalyco/opencode/commit/973715f3da1839ef2eba62d4140fe7441d539411), [OpenCode Anthropic provider documentation](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/web/src/content/docs/providers.mdx#L332-L369))

### What to learn, and what not to copy

**Inference:** OpenCode's current ChatGPT implementation is an emulation of the first-party Codex client surface, not the dynamic, per-user/workspace registration that OpenAI now documents for independent local applications. Public source does not prove whether OpenCode has a private approval or legacy entitlement. Therefore Arany must neither call it prohibited nor treat it as a generally authorized recipe.

**Recommendation:** reuse the product ideas—an explicit login method, browser and headless-aware UX, visible account attribution, refresh serialization, logout, and capability-filtered models. Do not reuse the static Codex client ID, device endpoints, private `backend-api` route, `localhost` callback, or token-import format. OpenAI's official current flow explicitly supplies different endpoints and says inference must use the public API rather than ChatGPT backend endpoints.

OpenCode's current provider copy describes this as ChatGPT Plus/Pro access. Arany should use OpenAI's prescribed “ChatGPT plan” language and decide eligibility from the actual direct-use scope because current plan availability can be broader and can change.

## OpenAI's official open-source/local flow

### Eligibility and registration

**Fact:** OpenAI's current program is an optional capability for open-source, locally hosted applications. The client begins with the special `client_id=dynamic_agent_client`, a stable opaque `ext_agent_host_id` for that installation, and a stable `agent_name_hint`, which should be `Arany`. No client secret or partner API key is used. OpenAI issues a real client ID, such as `oaiapp_...`, bound to the user and ChatGPT workspace during the callback; the application must persist that issued ID and never reuse `dynamic_agent_client` for subsequent token operations. ([OpenAI open-source token sharing overview](https://developers.openai.com/siwc/token-sharing-open-source), [OpenAI sign-in flow](https://developers.openai.com/siwc/token-sharing-open-source/sign-in))

**Recommendation:** generate the host identifier from cryptographically random bytes on first use and persist it in private application state. It identifies an Arany installation, not a Workspace, repository, user email, hostname, or machine fingerprint. Never derive it from attacker-controlled Workspace content or telemetry identifiers.

### Browser authorization

**Fact:** every authorization attempt must create fresh `state`, OIDC `nonce`, and PKCE verifier/challenge values. Arany must open the system browser at `https://auth.openai.com/api/accounts/authorize` with:

```text
client_id=dynamic_agent_client                 # first registration only
redirect_uri=http://127.0.0.1:<port>/auth/callback
response_type=code
code_challenge_method=S256
scope=openid profile email offline_access resource.invoke chatgpt.tokens.use.direct
resource=https://api.openai.com/v1
ext_agent_host_id=<stable opaque installation id>
agent_name_hint=Arany
state=<fresh random value>
nonce=<fresh random value>
code_challenge=<fresh PKCE S256 challenge>
```

The listener must be started before the browser is opened. The redirect URI must use the IP literal `127.0.0.1`; the documentation explicitly rejects `localhost`. Only the ephemeral port may change after registration. The callback returns the authorization code, state, and issued client ID for the first registration. Token exchange is at `https://auth.openai.com/api/accounts/oauth/token`. ([OpenAI sign-in flow](https://developers.openai.com/siwc/token-sharing-open-source/sign-in))

**Recommendation:** Arany should bind only `127.0.0.1`, accept exactly one bounded `GET /auth/callback`, require an exact `Host` consistent with the chosen port, reject unexpected query parameters or duplicates, compare `state` in constant time, enforce a short deadline, and close the listener on success, denial, timeout, or cancellation. It should not log the callback URI or render its values in the terminal or browser success page. A loopback listener is reachable by other local processes and by browser-origin requests, so PKCE, state, nonce, exact routing, and one-shot lifetime are all necessary.

### Token validation and account binding

**Fact:** after the token exchange, the ID token must be validated as an OIDC token: verify its signature through OpenAI's discovery/JWKS metadata, allowed algorithm, issuer, audience equal to the issued client ID, expiration, and the original nonce. The `sub` claim is the stable identity. The account must also possess the direct-plan scope; a valid identity token alone does not grant plan-funded inference. A returning login must match the saved registration identity rather than silently attaching a different account to the record. ([OpenAI sign-in flow](https://developers.openai.com/siwc/token-sharing-open-source/sign-in))

**Recommendation:** OIDC verification is a fail-closed boundary implemented with a mature, narrowly configured library. Bound discovery/JWKS response sizes and timeouts, allow only the compiled OpenAI origins, cache keys with bounded lifetime, support ordinary key rotation, and never accept an algorithm or issuer merely because the token names it.

### Refresh, rotation, profiles, and logout

**Fact:** OpenAI documents access tokens with a one-hour lifetime and refresh tokens with a 30-day lifetime. A successful refresh rotates the refresh token and restarts its 30-day lifetime. Refresh uses the same token endpoint, the issued client ID, the latest refresh token, and the API resource; it omits the original scope request. Applications must serialize refresh for a credential record so concurrent requests do not race rotating tokens. ([OpenAI token reference](https://developers.openai.com/siwc/token-sharing-open-source/token-reference), [OpenAI profiles and sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions))

**Fact:** separate ChatGPT accounts and workspaces require separate local records, even if human-readable emails match. Logout revokes the refresh token at the `revocation_endpoint` advertised by OpenAI's OIDC discovery document, with the issued client ID, and then removes local credentials. OpenAI says access, refresh, and ID tokens must not enter browser storage, URLs, version control, logs, analytics, or support bundles. ([OpenAI profiles and sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions))

**Recommendation:** one in-process mutex per credential record is the minimum. The persistence transaction must atomically replace the complete token set before another request can use it. A crash must yield either the old complete set or the new complete set, never a mixed access/refresh/client tuple. A future daemon or multiple simultaneous Arany processes would require an inter-process ownership/locking design; beta may instead refuse concurrent mutation of the same record.

### Inference route and capability limits

**Fact:** plan-funded inference uses the bearer token on public `POST https://api.openai.com/v1/responses`, never `chatgpt.com/backend-api`. Requests must set `store: false` and `stream: true`; success exists only when the stream reaches terminal `response.completed`, because an error may arrive after streaming begins. Models available to that account are listed at public `GET /v1/models`; Arany should expose only entries whose visibility permits listing and submit the documented model slug. ([OpenAI models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference))

**Fact:** the preview excludes or constrains many normal Responses features, including background execution, Conversations, `previous_response_id`, `max_output_tokens`, `max_tool_calls`, metadata, moderation, multi-agent mode, stored prompts, prompt-cache retention, safety identifiers, sampling controls, truncation, and `user`. It supports a bounded tool subset, including function/custom tools, but not every hosted tool. Full conversation input is sent on each request. ([OpenAI preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations))

**Inference:** a successful login proves identity and billing eligibility, not Arany's semantic Provider contract. Model listing proves availability, not strict structured-output conformance. The official documentation does not by itself prove that every eligible account/model accepts Arany's exact strict `Delegate | Finish` schema through this preview.

**Recommendation:** before beta support is enabled, run an opt-in live conformance test against the selected account and model. Prefer the same strict `text.format` schema as API-key mode if the plan route accepts it. A synthetic forced strict function/custom tool may be considered only if OpenAI's current docs and live response prove forced selection and strict arguments for that route. If neither encoding is provider-enforced, reject the profile; never use prompt-only JSON, local repair, or an extra hidden model call.

### Streaming and the aggregate token-budget blocker

**Fact:** the ChatGPT-plan route requires `store: false` and `stream: true`. It rejects `max_output_tokens` and `truncation`; those fields must be omitted. A request is successful only after `response.completed`. `response.failed`, `response.incomplete`, a broken stream, or EOF without `response.completed` is failure. An account usage-limit failure can arrive after the response has already begun streaming. ([OpenAI models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference), [OpenAI preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations))

Arany's current beta contract is different in two relevant ways:

- every Provider request is foreground and non-streaming;
- every request sets a provider-enforced `max_output_tokens: 4096`, and admission reserves the full semantic call budget before `RunStarted`: one call for a direct answer or `N + 2` calls for an admitted team Run.

**Inference:** mandatory wire streaming does not require Engine or terminal streaming. The OpenAI adapter can privately consume SSE, accumulate only the bounded semantic result, wait for `response.completed`, and then return one atomic `ProviderResponse`. This changes the adapter transport and fixtures but not the Provider interface or user presentation.

Reqwest's `Response::chunk()` incrementally reads a body without its optional `stream` feature, so the adapter does not need `futures-util`, `reqwest`'s `stream` feature, or an EventSource client. A private bounded decoder can implement the WHATWG event-stream grammar over arbitrary chunk boundaries. It must cap the aggregate ingress, line length, event bytes, event count, JSON nesting and semantic output; accept UTF-8 only; handle CRLF/LF/CR, comments and multi-line `data`; reject malformed or oversized events; ignore only syntactically valid unknown event types; prohibit reconnect; and require exactly one terminal state. ([Reqwest response body chunks](https://docs.rs/reqwest/latest/reqwest/struct.Response.html#method.chunk), [WHATWG server-sent event parsing](https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream))

**Fact:** crates such as `reqwest-eventsource` are built around reconnect policies, and its default is a retry policy. Reconnecting a `POST /v1/responses` request would risk a duplicate paid inference and violate Arany's admitted-call-budget/no-retry contract. `eventsource-stream` parses a byte `Stream` but requires reqwest's byte-stream surface and does not provide Arany's aggregate/event limits as the authoritative admission boundary. ([reqwest-eventsource retry policies](https://docs.rs/reqwest-eventsource/latest/reqwest_eventsource/retry/), [eventsource-stream](https://docs.rs/eventsource-stream/latest/eventsource_stream/))

**Recommendation:** use the private bounded decoder above if the profile is eventually admitted. Do not use an automatically reconnecting EventSource client and do not forward partial model text to Run state or presentation.

The token cap is not similarly adaptable. A local byte limit, schema string length, 120-second deadline, or dropped HTTP connection bounds Arany's memory and accepted result, but none proves that remote generation, reasoning-token use, or subscription allowance stopped at 4,096 tokens. Strict JSON Schema also cannot bound hidden reasoning. Prompting the model to be brief is probabilistic, not authority enforcement.

**Decision:** plan-funded inference cannot honestly ship under the current beta invariants. Authentication being official does not compensate for the missing provider-enforced token ceiling.

There are only two honest release paths:

1. **Preferred, no invariant change:** OpenAI adds a documented server-enforced output-token limit, or an equivalent enforceable allowance, to this route. Arany sends 4,096 on every admitted semantic call and live direct/team conformance proves that over-limit responses terminate as a typed failure. The aggregate reservation remains `4,096` for a direct Run or `(N + 2) × 4,096` for a team Run.
2. **Explicit product/security change:** an approved ADR replaces the universal output-token ceiling with a provider-profile capability. The ChatGPT profile would guarantee only the admitted number of request starts, fixed request/run deadlines, bounded local SSE/semantic bytes, and no retries; it would explicitly disclaim a deterministic remote output-token or subscription-allowance ceiling. Every canonical per-call and aggregate-budget claim and test would need to change. This is not an adapter implementation detail and is not approved by this report.

Until one path is approved and its tests pass, the capability snapshot for `OpenAI + ChatGptPlan` is `authentication_supported=true`, `inference_supported=false`, with no selectable Provider profile. Shipping an auth-only preview is not recommended because it adds secret, listener, cryptographic, and process surface without letting the user complete an Arany Run.

### Limits and billing UX

**Fact:** OpenAI documents specific errors for ineligible plans, missing consent, exhausted plan usage, unsupported features, incorrect routes, invalid users, and temporary service failures. Plan limits are account-specific and shared with other eligible clients. There is no provider push notification when eligibility is later removed; clients discover the change during refresh or inference. ([OpenAI errors and recovery](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery), [OpenAI profiles and sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions))

**Fact:** OpenAI's prescribed action is labeled “Continue with ChatGPT.” After consent, the product should clearly say “Using ChatGPT plan,” distinguish that mode from API billing, provide a route to ChatGPT usage settings, and direct usage-limit errors to plan management. ([OpenAI UI and UX guidance](https://developers.openai.com/siwc/token-sharing-open-source/ui-ux-guidelines))

**Recommendation:** a user declining direct-plan consent is not an authentication failure; it means plan-funded inference is unavailable. Offer the explicit API-key mode. Never fall back automatically from a plan-limit or eligibility error to an API key, because that can turn a bounded subscription action into metered API spend.

## Anthropic's explicit third-party boundary

### Native Claude Code behavior is not a reusable grant

**Fact:** official Claude Code supports browser authentication and a manual code path for headless environments. On macOS, Claude Code prefers Keychain with a private-file fallback; on Linux it uses `~/.claude/.credentials.json` with mode `0600`; Windows uses a profile-protected file. Claude Code manages refresh itself. These facts describe Anthropic's own client, not a credential-sharing API. ([Claude Code identity and access](https://code.claude.com/docs/en/iam))

**Fact:** Anthropic's current legal/compliance guidance says OAuth is exclusively for Claude subscriptions and ordinary Claude Code/native applications. Developers building products or services, explicitly including Agent SDK products, should use an API key or supported cloud provider. Third parties may not offer Claude.ai login, route Free/Pro/Max credentials, or collect, store, or intermediate Claude.ai credentials or session tokens without prior approval. The narrow exception is an end user authenticating an unmodified Claude Code binary embedded under Anthropic's stated commercial and branding conditions. Anthropic may enforce this policy without notice. ([Claude Code legal and compliance](https://code.claude.com/docs/en/legal-and-compliance), [Claude Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview))

**Fact:** Anthropic's consumer terms prohibit credential sharing and automated access except through an API key or another explicitly permitted mechanism. ([Anthropic consumer terms](https://www.anthropic.com/legal/consumer-terms))

**Recommendation:** Arany must not:

- display “Sign in with Claude” or reproduce Claude Code's browser/device flow;
- copy a Claude Code OAuth client ID, setup token, prompt headers, or client-identifying headers;
- read, import, watch, or refresh `~/.claude/.credentials.json`, Keychain entries, browser sessions, or environment-exported subscription tokens;
- proxy a user's Pro/Max token through Arany's native Anthropic Provider;
- recommend a community plugin as a supported beta path.

### The Agent SDK notice does not reverse the restriction

**Fact:** a separate Anthropic support notice says a planned billing change was paused and that, for now, certain Agent SDK, `claude -p`, and third-party Agent SDK usage can still draw from a user's subscription. ([Anthropic Agent SDK subscription notice](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan))

**Inference:** this describes current accounting for eligible, approved, or official SDK/CLI paths; it is not a blanket permission for an unrelated application to implement its own Claude OAuth flow. The more specific current legal/compliance and SDK documentation still requires prior approval for third-party subscription login and rate limits.

**Recommendation:** if Arany later evaluates Claude Code/Agent SDK delegation, treat it as a separate integration rather than an Anthropic Provider credential mode. It would run another harness that owns model interaction, tools, prompting, memory, and possibly subprocess authority, conflicting with Arany's current deep Engine and bounded semantic proof. Require written Anthropic approval, Commercial Terms review, unmodified-binary provenance, explicit user authentication, a new process/security threat model, and an architecture decision before prototyping it.

## Comparable official clients

### OpenAI Codex CLI

**Fact:** the official Codex CLI distinguishes API-key and ChatGPT OAuth authentication, owns token refresh/revocation, and offers configurable persistence modes including file, keyring, automatic selection, and ephemeral storage. Its login server uses a local callback and PKCE/state, and its structured logging takes care around secrets. ([Codex authentication protocol types](https://github.com/openai/codex/blob/main/codex-rs/protocol/src/auth.rs), [Codex authentication manager](https://github.com/openai/codex/blob/main/codex-rs/login/src/auth/manager.rs), [Codex login server](https://github.com/openai/codex/blob/main/codex-rs/login/src/server.rs), [Codex credential-store configuration](https://github.com/openai/codex/blob/main/codex-rs/config/src/types.rs))

**Recommendation:** Codex is a useful implementation-quality reference for Rust lifecycle, redaction, refresh, and keyring fallback. It is not a source of reusable OAuth identity. Arany must implement OpenAI's third-party dynamic-registration specification, not copy Codex's first-party client ID or backend routing.

### Aider

**Fact:** Aider's official documentation treats provider API keys as separately billed credentials and warns that a ChatGPT subscription is not an OpenAI API subscription. It does not present consumer-session import as its ordinary OpenAI integration. ([Aider optional installation and provider billing](https://aider.chat/docs/install/optional.html))

**Inference:** the safe common denominator among independent harnesses has historically been API keys. OpenAI's new dynamic registration creates a legitimate, narrower exception. Anthropic has not created an equivalent general third-party path.

## Rust implementation choices

This section closes the dependency choices for a future implementation. Because plan-funded inference is currently release-blocked, these dependencies should not enter the initial beta merely to support an unusable login.

### OAuth2, OIDC, JWKS, and nonce validation

**Recommendation:** use `openidconnect` 4.0.1 as the sole direct OAuth/OIDC protocol dependency. It is built on `oauth2` 5.0.0 and already implements Authorization Code plus PKCE, discovery, JWKS-backed ID-token verification, nonce validation, refresh, and revocation. Do not add `oauth2`, a JWT crate, or hand-written JOSE as additional direct dependencies. The upstream example demonstrates discovery, fresh PKCE/state/nonce, code exchange, ID-token claims validation, and optional `at_hash` verification. ([openidconnect 4.0.1](https://docs.rs/openidconnect/4.0.1/openidconnect/), [oauth2 5.0.0](https://docs.rs/oauth2/5.0.0/oauth2/))

Use this feature shape when the release gate opens:

```toml
openidconnect = { version = "4.0.1", default-features = false, features = ["timing-resistant-secret-traits"] }
```

Disabling default features is deliberate. `openidconnect` 4.0.1's defaults enable its own reqwest 0.12 and rustls integration, while Arany's reviewed HTTP stack uses reqwest 0.13. The crate documents a custom asynchronous HTTP-client interface over `http::Request<Vec<u8>>` and `http::Response<Vec<u8>>`; use a small adapter around Arany's existing hardened reqwest client instead of resolving two reqwest/TLS stacks. The OIDC documentation itself warns that following redirects creates SSRF risk. ([openidconnect features](https://docs.rs/crate/openidconnect/4.0.1/features), [openidconnect custom HTTP clients and security warning](https://docs.rs/openidconnect/4.0.1/openidconnect/#importing-openidconnect-selecting-an-http-client-interface))

The auth client is separate from the inference client but comes from the same private hardened builder. It has:

- redirects, cookies, ambient proxies, referer insertion and retries disabled;
- exact HTTPS origins and paths for discovery, JWKS, authorization metadata, token exchange and revocation;
- connect, request and body-read deadlines;
- bounded discovery, JWKS, token and error bodies read incrementally before deserialization;
- sensitive authorization and token headers, and no body/header debug logging.

For initial authorization, construct `CoreClient` with `dynamic_agent_client`. After the callback returns the issued client ID, construct a new client from the already validated provider metadata and that issued ID for code exchange and ID-token verification. Add the documented `resource`, host ID and initial agent-name parameters through typed fixed helpers, not a generic user-controlled extra-parameter map.

**Fact:** OpenAI's current discovery document fixes issuer `https://auth.openai.com`, the documented authorization/token/revocation/JWKS endpoints, Authorization Code and refresh grants, PKCE `S256`, public-client authentication method `none`, and ID-token algorithm `RS256`. ([OpenAI OIDC discovery](https://auth.openai.com/.well-known/openid-configuration))

**Recommendation:** discovery is input, not authority. After `CoreProviderMetadata::discover_async`, require exact equality with the compiled issuer, authorization, token, revocation and JWKS URLs; require code flow, S256 and RS256; and reject extra or missing values that would widen the accepted profile. Configure the verifier to accept RS256 only. Refresh the JWKS once on an unknown `kid`, under the same byte/time limits, then fail closed.

The crate's standards table says `azp` verification is unsupported. Avoid that gap by requiring the validated ID token's audience list to contain exactly one value, the issued client ID; reject multi-audience tokens rather than attempting custom `azp` logic. Verify `at_hash` when present, as the upstream example does. ([openidconnect supported and unsupported standards](https://docs.rs/crate/openidconnect/4.0.1))

The `timing-resistant-secret-traits` feature exists so state and other secret-wrapper comparisons do not become an ordinary early-exit string comparison. It is preferable to adding another direct constant-time crate. No secret type may derive or implement informative `Debug`, `Display`, serialization to Events, or cloning beyond the narrow flow that needs it.

### Minimal loopback callback

**Recommendation:** use Tokio's `TcpListener` and I/O extension traits plus `httparse` 1.10.1. Do not add Hyper, Axum, Tower HTTP, a general web server, or a second runtime for a one-route, one-transaction callback. Tokio documents `net` and `io-util` as separable features and supports them on its current-thread scheduler. `httparse` is a small HTTP/1 parser with no runtime dependencies; its internal unsafe and build script still require the normal dependency review. ([Tokio feature flags and networking](https://docs.rs/tokio/latest/tokio/), [httparse 1.10.1](https://docs.rs/httparse/1.10.1/httparse/))

```toml
httparse = "1.10.1"
tokio = { version = "1.53", default-features = false, features = ["io-util", "net", "rt", "signal", "sync", "time"] }
```

Before accepting that addition, `cargo tree` must confirm whether reqwest already resolves the same `httparse`; a second version is a release failure unless justified. Application code contains no `unsafe`.

The callback server is an `arany auth login`-only capability created before any Workspace input:

- bind the IP literal `127.0.0.1` with port `0`, read the assigned port, and keep the exact URI for authorization and exchange;
- own one pending transaction, a five-minute total deadline, at most eight accepted connections, a two-second per-connection read deadline, a 16 KiB request-head cap, 32-header cap and 8 KiB request-target cap;
- accept only HTTP/1.1 `GET /auth/callback?...`, an exact single `Host: 127.0.0.1:<port>`, no `Origin` header, no request body or transfer encoding, and no duplicate security or OAuth query keys; the valid top-level redirect is verified in real Safari, Chrome and Firefox lanes before release;
- allow only the documented `code`, `state`, `client_id`, `scope`, `error`, and `error_description` keys; require mutually exclusive success/error shapes and validate `state` before displaying even a denial;
- treat malformed and unsolicited requests as bounded 400 responses and continue until the valid callback, connection cap or deadline; complete at most once;
- return a fixed inert success/denial page with `Cache-Control: no-store`, `Content-Security-Policy: default-src 'none'; frame-ancestors 'none'`, `X-Content-Type-Options: nosniff`, no reflected values and no script;
- drop the listener and zero/drop authorization code, state, nonce and PKCE verifier immediately after exchange or failure.

This is a narrow exception to the current “no listener” beta boundary, not a reusable local service. The canonical security contract must explicitly approve that exception before implementation.

### System browser opening

**Fact:** OpenAI says to open the system browser after the listener is ready. The `webbrowser` 1.2.4 crate supports Linux and macOS and has a `hardened` feature that rejects non-HTTP(S) targets. However, its Linux implementation first interprets the ambient `BROWSER` variable, searches `PATH`, reads desktop configuration, and spawns discovered commands; fallback paths include `xdg-open`, desktop-specific programs and `x-www-browser`. ([OpenAI sign-in flow](https://developers.openai.com/siwc/token-sharing-open-source/sign-in), [webbrowser 1.2.4](https://docs.rs/crate/webbrowser/1.2.4), [webbrowser Linux source](https://github.com/amodm/webbrowser-rs/blob/v1.2.4/src/unix.rs))

**Decision:** do not add `webbrowser` to Arany's beta dependency set. Its ambient executable selection conflicts with Arany's current trusted-executable/no-PATH rules, and any automatic opener introduces the subprocess authority that the current beta explicitly excludes. The `hardened` feature constrains the URL scheme; it does not constrain executable provenance.

A plain authorization URL can be printed as a manual recovery path without OSC hyperlinks, but this report does not claim that manual-only behavior satisfies OpenAI's instruction to open the system browser. Before subscription authentication ships, choose one of these through an explicit security/product decision:

1. approve a narrowly scoped platform opener for `auth login`, with fixed platform integration, generated `https://auth.openai.com/...` input only, no shell, no repository or `BROWSER`/PATH selection, bounded lifecycle and an inert manual fallback; or
2. obtain confirmation that a manual browser handoff is acceptable and ship no process opener.

Until then, browser launch is a second release blocker independent of the output-token cap. Do not paper over it by calling `webbrowser::open` from the coordinator or an unbounded blocking task.

### macOS and Linux credential storage

**Recommendation:** when the profile becomes shippable, default to `keyring` 4.2.0's `v1` facade. It selects macOS Keychain Services for an unsigned CLI and the freedesktop Secret Service through zbus on Linux. The crate's `v1` feature exposes get/set/delete of text or binary secrets, has Rust 1.88 MSRV matching Arany's existing floor, and uses target-specific platform dependencies. Do not use Apple's protected-data store: its upstream documentation says command-line tools lack the provisioning-profile entitlements it expects. ([keyring 4.2.0 manifest](https://github.com/open-source-cooperative/keyring-rs/blob/v4.2.0/Cargo.toml), [keyring v1 facade](https://docs.rs/keyring/4.2.0/keyring/v1/), [Apple native keyring stores](https://docs.rs/apple-native-keyring-store/1.0.2/apple_native_keyring_store/))

```toml
keyring = { version = "4.2.0", default-features = false, features = ["v1"] }
```

Store one serialized, versioned credential record as one secret under service `dev.arany.cli.openai-chatgpt` and an opaque random local profile ID. Keeping access token, rotating refresh token, ID token, issued client ID, subject, granted scopes, expiry and host ID in one entry prevents a reader from observing a deliberately mixed tuple. A private, owner-only, non-secret profile index maps the local profile ID to a display label and the selected store; it contains no tokens, authorization code or raw provider response.

Keyring calls are synchronous and the underlying store may prompt, block, or behave poorly when one credential is accessed concurrently. They must not run on the current-thread Tokio coordinator. One bounded credential worker thread owns all get/set/delete operations, serializes refresh persistence and exits with the command/runtime; its request enum contains `secrecy`-wrapped bytes and has a redacted `Debug`. Do not use Tokio's general blocking pool. ([keyring-core thread-safety note](https://docs.rs/keyring-core/1.0.0/keyring_core/))

An in-process worker is not enough because two Arany processes can select the same profile. Raise the future feature's `rust-version` from 1.88 to 1.89 and use the standard library's `File::try_lock` on one no-follow, owner-only per-profile lock file beneath the private state root. Open it relative to the already admitted `cap_std::fs::Dir`, convert that owned handle with `into_std`, and lock the handle rather than reopening an ambient path. Rust 1.89 implements the lock with `flock` on Unix and releases it when the file handle closes. This avoids adding the older `fs2` FFI/unsafe dependency. Under the exclusive lock, re-read the credential record, refresh only if still necessary, replace the whole record, then release. Contention fails quickly as `credential profile busy`; it never starts a second refresh. The advisory lock coordinates Arany instances but is not a protection claim against malicious same-user software. ([Rust `File::try_lock`](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock), [cap-std `File::into_std`](https://docs.rs/cap-std/4.0.3/cap_std/fs/struct.File.html#method.into_std))

Linux Secret Service is not universally available, especially in headless sessions, and its upstream implementation documents headless and WSL limitations. Therefore a file store is necessary, but fallback must never be automatic. ([zbus Secret Service store](https://docs.rs/zbus-secret-service-keyring-store/1.0.1/zbus_secret_service_keyring_store/))

The exact fallback policy is:

- default `auth login` to `os`; a missing, locked, ambiguous, denied or unavailable OS store fails with a stable message and does not write a file;
- let the user explicitly re-run with `--credential-store file` after a warning that owner-only permissions are not encryption at rest;
- record `os` or `file` in the private profile index and never switch an existing profile implicitly;
- store the complete versioned credential record in one private file beneath Arany's already admitted state root, using no-follow handle-relative access, exclusive temporary creation, atomic rename, owner verification and `0600` on Unix;
- refuse symlinks, hard-link count other than one, non-regular objects, wrong ownership/mode, a state root inside the Workspace, and persistence on a filesystem that cannot meet the existing state guarantees;
- on successful token refresh, replace the entire record. If remote rotation succeeds but local persistence fails or the process crashes first, mark the profile unusable and require reauthorization; no local transaction can make a remote token rotation atomic.

There is no environment-variable credential blob, plaintext SQLite credential table, credential export command, or automatic migration from Codex/OpenCode/Claude stores.

### Exact candidate dependency delta

If and only if the token-budget and browser/security gates are approved, the smallest reviewed delta is:

```toml
[package]
rust-version = "1.89"

[dependencies]
httparse = "1.10.1"
keyring = { version = "4.2.0", default-features = false, features = ["v1"] }
openidconnect = { version = "4.0.1", default-features = false, features = ["timing-resistant-secret-traits"] }
tokio = { version = "1.53", default-features = false, features = ["io-util", "net", "rt", "signal", "sync", "time"] }
```

Existing `reqwest`, `serde`, `serde_json`, `secrecy`, and `sha2` cover HTTP adaptation, credential encoding/redaction and any required digest comparison. Rust 1.89 supplies the cross-process file lock. Reqwest `Response::chunk()` covers plan-route SSE without adding its `stream` feature. Explicitly absent are direct `oauth2`, JWT/JOSE, `url`, Hyper/Axum, `webbrowser`, EventSource/reconnect, async keyring, `fs2`, a second runtime, and an auth daemon.

`Cargo.lock`, `cargo tree -e features`, license/advisory review, build-script review and resolved duplicate checks remain release evidence. In particular, review `openidconnect`'s pure-Rust cryptographic dependency versions, the macOS Security.framework wrapper and Linux zbus/Secret Service stack, and `httparse`'s contained unsafe/build script. The fact that some are already transitive does not remove the review requirement when Arany begins relying on them directly.

## Arany architecture recommendation

### Keep authentication below the Provider seam

Authentication changes how the selected OpenAI adapter acquires a bearer token and which compiled endpoint profile it may call. It does not change the Engine's semantic request or response.

```text
CLI auth commands
    |
    v
OpenAI credential profile ----> secret store
    |                               |
    | opaque profile handle         | access only inside adapter
    v                               v
Provider construction ------> OpenAI adapter ------> compiled OpenAI origin
                                  |
                                  v
                         validated Delegate | Finish
                                  |
                                  v
                               Engine
```

**Recommendation:** if the release gates open, add `ChatGptPlan` as an internal OpenAI authentication/profile variant, not a new Provider, Engine seam, crate, service, or daemon. Keep it private in `provider.rs` until OIDC, listener, storage, and lifecycle code has enough depth to justify a private `auth.rs` module. The Engine sees only the already-selected, capability-conformant Provider.

The profile should bind at least:

```text
OpenAiCredentialProfile {
    local_profile_id,
    mode: ApiKey | ChatGptPlan,
    account_subject,
    workspace_identity,
    issued_client_id,
    external_agent_host_id,
    granted_scopes,
    token_expiry,
    secret_handle,
}
```

Only an opaque local profile ID, mode, non-sensitive account label, and capability status may reach ordinary configuration or presentation. Raw ID, access, and refresh tokens remain behind the secret handle.

### Secret-state location and persistence

**Recommendation:** consumer OAuth state is mutable secret operational state. It must not enter:

- the Workspace or its `.env` files;
- SQLite canonical Events, replay records, memory, prompts, or artifacts;
- JSONL/human output, errors, crash reports, logs, or OTLP attributes/events;
- child-agent context or provider-visible input;
- command-line arguments or shell history.

Use the operating-system credential store by default and the explicit, never-automatic file fallback defined in the Rust implementation section. Metadata may be separate from tokens only if both parts are correlated and neither makes an incomplete record appear valid.

**Inference:** placing OAuth tokens in the agent-event SQLite database would simplify persistence but expand the canonical state reader, backup, export, diagnostics, and replay threat surfaces. Tokens have different retention, redaction, rotation, and deletion semantics, so that convenience violates the current state boundary.

### Gated future command shape

```text
arany auth login openai --mode chatgpt
arany auth status openai
arany auth logout openai

# Existing non-interactive API-key configuration remains separate.
arany ... --provider openai --auth-profile <PROFILE>
```

These commands remain unavailable while inference is release-blocked. Their precise future shape remains a product decision, but the following behavior is mandatory if enabled:

- login states the external browser, account identity, granted billing mode, and data-sharing implications before success;
- status never refreshes or contacts a provider unless explicitly documented, and never prints tokens;
- logout attempts remote revocation, reports its result without secrets, and removes or quarantines local material according to an explicit failure policy;
- a Run resolves exactly one profile before Workspace input and never reads unrelated credentials;
- environment API keys do not silently override a selected ChatGPT profile, and a ChatGPT failure does not silently fall back to them;
- account switches are explicit and accepted only after full validation of the new record.

### Endpoint and data policy

The plan profile has one compiled inference origin, `https://api.openai.com`, one public path family, and the OpenAI OIDC endpoints required by the official specification. It must disable arbitrary base URLs, redirects, ambient proxies, cookies, and repository-provided network configuration. Authorization permits calls to these endpoints; it does not authorize disclosure of arbitrary Workspace or credential data.

OpenAI states that consumer ChatGPT content may be used to improve models depending on the account and data-control settings, whereas business offerings have different defaults. Arany must not describe ChatGPT-plan privacy as equivalent to API or enterprise policy. ([OpenAI: using Codex with a ChatGPT plan](https://help.openai.com/en/articles/11369540-using-codex-with-your-chatgpt-plan))

## Beta release gates

The first gate is architectural, before the detailed checks below: either the official route gains a provider-enforced 4,096-token output ceiling, or an approved ADR explicitly removes that universal beta guarantee and replaces every affected security, budget and UX claim. The second gate approves a compliant system-browser mechanism and the narrow auth-only listener/process exceptions. Without both, no ChatGPT-plan commands or dependencies ship.

### Legal and product eligibility

- Re-check the official OpenAI token-sharing overview, sign-in, limitations, and UX requirements at the release commit.
- Confirm Arany is still open source and locally hosted under the documented eligibility. Before any paid, closed-source, centrally hosted, or remote-token-broker offering, pause support and use OpenAI's stated approval/contact process.
- Re-check Anthropic legal/compliance and SDK guidance. Direct Claude subscription auth remains disabled unless Anthropic gives explicit written approval for Arany's exact design.
- Document that ChatGPT-plan and API-key billing, limits, retention, and data controls are distinct.

### OAuth and local-listener verification

- State, nonce, and PKCE values are cryptographically random, unique per attempt, bound to one transaction, and never logged.
- Tests reject missing, duplicate, expired, replayed, or mismatched state; nonce mismatch; wrong issuer/audience/client ID/resource/scope; expired tokens; unknown or disallowed signing algorithms; and invalid signatures.
- JWKS rotation and unknown-key recovery are bounded and fail closed without accepting unverified claims.
- The listener binds only `127.0.0.1`, starts before browser launch, accepts only the exact method/path/Host and bounded query, serves one transaction, has a short timeout, and cleans up on every cancellation/error path.
- Adversarial tests cover DNS-rebinding-style Host values, unsolicited browser requests, duplicate callback values, oversized URLs/headers, port races, slow clients, and concurrent login attempts.
- Headless environments receive an honest unsupported/manual workflow derived from current official documentation; Arany must not copy OpenCode's Codex device endpoints unless OpenAI documents them for dynamically registered third-party clients.

### Secret lifecycle verification

- No token appears in terminal output, JSONL, SQLite, logs, telemetry, panic text, HTTP diagnostics, test snapshots, fixtures, process arguments, environment dumps, or support bundles.
- File fallback tests cover links, wrong owner/mode, special files, predictable temporary names, interrupted atomic replacement, partial writes, disk full, and crash between refresh and persistence.
- macOS Keychain and Linux Secret Service lanes cover set/get/replace/delete, locked/denied/unavailable/ambiguous stores and explicit file fallback without ever silently changing stores.
- Refresh is single-flight per record and across two Arany processes; the loser re-reads under the per-profile lock or fails `credential profile busy`, never uses a superseded refresh token and never publishes a mixed token set.
- Account switch, logout, revocation failure, expired refresh, revoked consent, and local deletion each have explicit, tested state transitions.
- Retention is minimal: discard authorization code, PKCE verifier, state, and nonce as soon as the transaction completes; delete tokens on logout and provide a deterministic recovery path if remote revocation is unavailable.

### Provider conformance verification

- The request uses a documented provider-enforced 4,096-token ceiling and preserves the 16,384-token Run reservation, unless a separately approved ADR has intentionally replaced that invariant.
- A pinned eligible account/model passes the strict outcome schema for both a direct answer and one admitted small-team Run, with every semantic call accounted for.
- Request fixtures prove `store: false`, `stream: true`, the public `/v1/responses` route, only preview-supported parameters, the exact model, and no hidden call beyond the admitted semantic budget.
- SSE fixtures cover success only at `response.completed`, provider error after HTTP success, malformed/oversized events, unknown events, disconnect, refusal, truncation, duplicate outcomes, and cancellation.
- Live opt-in tests prove the exact outcome encoding and current model availability without recording prompts or credentials.
- Usage exhaustion, consent loss, model disappearance, unsupported feature, refresh expiry, and temporary provider failure map to stable errors; none triggers API-key or cross-provider fallback.
- Egress canaries prove omitted Workspace files, unrelated credentials, tokens, state records, and telemetry never reach OpenAI.

### User-visible verification

- Golden terminal and JSONL tests show the selected provider, model, account/profile label, and `API billing` versus `Using ChatGPT plan` without exposing sensitive identifiers.
- Login denial is distinguished from transport failure and from plan-consent denial.
- Limit errors provide the documented management action and do not suggest that API billing is included.
- An API-key run behaves exactly as before when ChatGPT support is unconfigured.

## Risks and explicit non-claims

- Official OAuth makes the route authorized; it does not make tokens harmless. A refresh token is long-lived account authority and must receive stronger handling than an ordinary cache entry.
- Loopback is not authentication. Another local process can race or probe the listener; transaction-bound state, nonce, PKCE, exact routing, and listener lifetime are the defenses.
- `0600` is access control, not encryption and not protection against malware running as the same user.
- OpenAI's plan-sharing feature is a preview with a constrained capability surface. Arany cannot promise every ChatGPT plan, model, account, region, or future API feature.
- Model catalogs do not prove semantic conformance, privacy, price, or permanence.
- Claude Code accepting Pro/Max is not evidence that Arany may accept the same token.
- Running an unmodified provider CLI could be legally distinct from stealing its credentials but is also architecturally distinct from a native Provider. This report does not approve that integration.
- Neither provider permits credential sharing. Arany profiles are local to the authenticated user and must not be exported, synchronized, proxied, or made available to subagents as raw secrets.

## Final recommendation

Do not implement or advertise **OpenAI — Using ChatGPT plan** in the current beta. The official authentication flow is sound enough to design, but the inference route rejects Arany's required server-side token cap, and automatic system-browser opening conflicts with the current process trust boundary. Keep OpenAI API-key mode as the beta path.

Reconsider the subscription profile only when the token-budget gate is preserved or explicitly redesigned and the browser/listener security exception is approved. At that point, use only OpenAI's dynamic-registration OAuth, public Responses API, bounded private SSE reducer, `openidconnect`-based validation, OS-keyring-first storage and explicit file fallback described here. The strict-outcome live gate remains mandatory.

Keep **Anthropic — API key** as the only native Anthropic beta mode. Do not implement direct Claude subscription login, credential import, or Claude Code impersonation. Revisit an official Claude CLI/Agent SDK bridge only after Anthropic approval and a separate decision about importing another agent loop.

This postpones a desirable feature for a concrete deterministic reason, not because consumer authentication is impossible. It also avoids treating reverse-engineered first-party behavior as a portability standard.
