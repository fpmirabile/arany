# Consumer subscription authentication: evidence and trade-offs

Source baseline: 2026-09-29. This retained comparison does not revalidate current provider policy or establish live-account conformance.

[ChatGPT spec](../specs/chatgpt.md), [credential behavior](../specs/credentials.md) and [Provider spec](../specs/provider.md) own the implementation. The [subscription recheck](./subscription-provider-routes-2026-10.md) records later policy evidence; [D-18](./next-step-decision-register.md#d-18--explicitly-accepted-chatgpt-plan-route-is-beta-scope) records the accepted weaker remote-cost guarantee. This report retains the reasons and source comparisons, not the original implementation plan.

## Billing and permission are separate

The sources inspected on the baseline date distinguished ordinary API billing from consumer subscriptions:

**Fact:** Claude Pro, Max, Team, and Enterprise subscriptions do not include Anthropic Console/API access or API credits. Console billing is separate. Pro and Max can include Claude Code, but Claude Code is Anthropic's own product and its subscription authentication is not a general-purpose API credential. ([Anthropic: subscription and API billing are separate](https://support.claude.com/en/articles/9876003-i-have-a-paid-claude-subscription-pro-max-team-or-enterprise-plans-why-do-i-have-to-pay-separately-to-use-the-claude-api-and-console), [Claude Code with Pro or Max](https://support.claude.com/en/articles/11145838-use-claude-code-with-your-pro-or-max-plan))

**Fact:** ChatGPT and the OpenAI API normally have separate billing systems. A ChatGPT subscription does not generally fund API usage. ([OpenAI: ChatGPT and API billing](https://help.openai.com/en/articles/9039756-managing-billing-for-chatgpt-and-the-api-platform))

OpenAI's documented exception covered eligible open-source, locally hosted applications using a constrained public Responses API subset. It did not expose ChatGPT history or arbitrary account data. Eligibility for that program was narrower than simply having a ChatGPT subscription. [OpenAI open-source token sharing](https://developers.openai.com/siwc/token-sharing-open-source).

Anthropic's inspected legal/compliance and SDK guidance reserved subscription OAuth for approved or first-party paths; its support notice about paused SDK billing changes described accounting behavior, not a general third-party OAuth grant. This distinction explains why successful token use was insufficient evidence of permission. Later policy interpretation belongs to the subscription recheck linked above. [Legal/compliance guidance](https://code.claude.com/docs/en/legal-and-compliance), [Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview), [SDK subscription notice](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan), [consumer terms](https://www.anthropic.com/legal/consumer-terms).

Delegating to an unmodified CLI/SDK also imports another harness's model loop, tools and process authority. It is a different architecture from admitting a credential to Arany's Provider; an authentication comparison alone cannot settle that decision.

## Pinned OpenCode comparison

The observations below refer to OpenCode commit [`7945de2`](https://github.com/anomalyco/opencode/commit/7945de208964a49300d7f770d1a71d078db9a4c4), not an assertion about its latest release.

Its bundled adapter used the static Codex OAuth identity, a localhost callback and the private ChatGPT backend inference route, while OpenAI's independent-app documentation described dynamic registration and public API inference. That difference is evidence against treating source compatibility as a transferable authorization. Public code alone does not establish whether OpenCode had separate approval. [Pinned Codex adapter](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/plugin/openai/codex.ts).

The inspected adapter serialized refresh within one process and bounded the browser callback, but its device polling had no visible overall deadline. It used PKCE/state; no OIDC nonce or signature validation was visible in that adapter. These observations concern the inspected layer, not a whole-product vulnerability finding. [Pinned adapter](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/plugin/openai/codex.ts).

Its credential module persisted plaintext tokens in a mode-0600 file; the inspected module showed no OS keyring, explicit cross-process locking or explicit atomic replacement. File permissions and refresh serialization solve different problems. [Pinned authentication storage](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/auth/index.ts).

**Fact:** OpenCode removed its bundled `opencode-anthropic-auth` plugin, the “Claude Max or API key” option, and Claude Code-specific prompt/header behavior in the 2026-02-19 commit titled “anthropic legal requests.” The inspected provider documentation says Anthropic prohibits this subscription-auth mechanism and any remaining community plugins are not bundled. ([OpenCode removal commit](https://github.com/anomalyco/opencode/commit/973715f3da1839ef2eba62d4140fe7441d539411), [OpenCode Anthropic provider documentation](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/web/src/content/docs/providers.mdx#L332-L369))

## Identity, inference and remote-cost guarantees

The reviewed OpenAI flow bound dynamic registration and verified identity to the account/workspace. Separate account records, serialized rotating refresh tokens and revocation were protocol concerns, not proof of model capabilities. A login or model listing could not establish strict structured-output conformance. [Sign-in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in), [token reference](https://developers.openai.com/siwc/token-sharing-open-source/token-reference), [profiles and sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions).

The inspected preview required streaming and rejected `max_output_tokens`. Local byte limits, a deadline, strict schemas and dropping the connection limit local processing or accepted output; they do not prove that remote generation or hidden reasoning stopped at an equivalent token budget. This is the reason for the distinct consented guarantee recorded in D-18, not a remaining release prohibition. [Models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference), [preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).

Streaming transport and incremental presentation are separate choices. Automatic EventSource reconnect can repeat an inference POST, whereas parsing an already admitted stream does not require reconnect. The reviewed `reqwest-eventsource` default retry policy explains why a convenience client was not itself the lifecycle contract. [Retry policies](https://docs.rs/reqwest-eventsource/latest/reqwest_eventsource/retry/), [WHATWG event-stream parsing](https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream). [Provider spec](../specs/provider.md) own the implemented behavior.

Plan limits were shared with other eligible clients, and eligibility could change between login, refresh and inference. Explicit billing attribution matters because substituting an API key after a plan error can incur separately metered spend. [Errors and recovery](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery), [UI guidance](https://developers.openai.com/siwc/token-sharing-open-source/ui-ux-guidelines).

## Library and credential-store trade-offs

The original dependency comparison found useful distinctions that survive the now-absorbed dependency plan:

- `openidconnect` 4.0.1 documented a custom HTTP-client interface and warned that redirects can enable SSRF. Its standards table did not support `azp` verification. A mature protocol library still leaves endpoint authority and the admitted identity profile to its caller. [HTTP interface and warning](https://docs.rs/openidconnect/4.0.1/openidconnect/#importing-openidconnect-selecting-an-http-client-interface), [standards coverage](https://docs.rs/crate/openidconnect/4.0.1).
- `webbrowser` 1.2.4's hardened feature constrained URL schemes; its inspected Linux implementation still selected executables from ambient browser settings, PATH and desktop configuration. Scheme validation therefore did not establish executable provenance. [Published crate](https://docs.rs/crate/webbrowser/1.2.4), [pinned Linux source](https://github.com/amodm/webbrowser-rs/blob/v1.2.4/src/unix.rs).
- Apple protected-data keyring storage depended on provisioning entitlements unsuitable for an ordinary unsigned CLI. Linux Secret Service availability depended on the session, including headless/WSL limitations. OS storage was therefore not a universal availability guarantee. [Apple native stores](https://docs.rs/apple-native-keyring-store/1.0.2/apple_native_keyring_store/), [Secret Service store](https://docs.rs/zbus-secret-service-keyring-store/1.0.1/zbus_secret_service_keyring_store/).
- Keyring operations were synchronous and could prompt or block. A worker thread alone cannot terminate a stuck native operation or make timeout equivalent to rollback. The later [credential-timeout investigation](./harness-credential-timeouts-2026-10.md) retains the process-ownership evidence; credential rules own the selected implementation. [Keyring concurrency note](https://docs.rs/keyring-core/1.0.0/keyring_core/).

Keeping a rotating credential tuple together avoids mixed local fields, but no local atomic replacement makes remote refresh-token rotation atomic with local persistence. Likewise, an advisory file lock coordinates cooperating processes without protecting against arbitrary malicious same-user code. These are failure-model limits, not a proposed record schema or migration path. [OpenAI refresh semantics](https://developers.openai.com/siwc/token-sharing-open-source/token-reference), [Rust file-lock semantics](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock).

No login, real token mutation, paid inference or native platform-conformance result is established by this report.
