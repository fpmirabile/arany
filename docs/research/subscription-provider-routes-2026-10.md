# Subscription Provider routes: October 2026 recheck

Retrieved: OpenAI 2026-10-02; Anthropic rechecked 2026-10-03  
Arany policy reconciled: 2026-10-04; provider-source retrieval dates unchanged  
Status: dated official-source evidence, not live account conformance or legal approval

This report distinguishes a documented third-party authorization from a first-party client that happens to accept a subscription. Neither route was exercised with a paid account for this report.

| Route | Current official position | Arany implication |
| --- | --- | --- |
| ChatGPT Plus/Pro in a local open-source app | OpenAI explicitly offers Sign in with ChatGPT plan usage to eligible users of open-source/local apps, with a dynamically issued client ID and a user-consented OAuth grant. | A native Arany account route is eligible to implement; login alone is not inference or model-conformance evidence. |
| Claude Pro/Max in Arany's native Anthropic adapter | Anthropic directs third-party product developers, including Agent SDK products, to API keys or a supported cloud provider. Without prior approval they may not offer Claude.ai login, intermediate Claude.ai credentials, or route plan credentials for their users. | Do not offer native Claude subscription login or import Claude Code tokens. Seek explicit Anthropic approval for that product path; retain the API-key adapter independently. |
| Unmodified Claude Code binary used by an end user | Anthropic describes a narrower permitted case under its commercial/product conditions: the end user signs in to the unmodified binary with their own account. | This is another agent runtime, not a drop-in credential source for Arany's Provider seam; it requires a separate architecture and security decision if considered. |
| Claude Code routine API trigger | A user-created, per-routine token starts a saved Claude Code web routine and returns its session URL, not a model result. | Useful only for that separate automation shape; it cannot satisfy Arany's native Provider outcome contract. |

## ChatGPT plan: authorization and cost distinction

The 2026-10-02 review of OpenAI's [open-source route](https://developers.openai.com/siwc/token-sharing-open-source)
and [registration guide](https://developers.openai.com/siwc/token-sharing-open-source/sign-in)
found dynamic registration and distinct user consent for eligible local apps.
The [public inference route](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference)
is not the private ChatGPT backend. Its [preview limits](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations)
exclude remote output-token caps: stopping local reading does not stop or price
all remote work. [D-18](./next-step-decision-register.md#d-18--explicitly-accepted-chatgpt-plan-route-is-beta-scope)
owns acceptance of that weaker guarantee; [ChatGPT](../specs/chatgpt.md) owns the
account, consent, catalog and route-specific behavior, while [Provider](../specs/provider.md)
admission and [execution](../specs/execution.md) outcome rules own the rest. Login and catalog
visibility alone establish neither model conformance nor live billing evidence.

## Claude plan: policy boundary, Pi comparison, and public alternative

- Rechecked 2026-10-03: Anthropic's [Claude Code legal and compliance guide](https://code.claude.com/docs/en/legal-and-compliance), [Agent SDK quickstart](https://code.claude.com/docs/en/agent-sdk/quickstart), and [Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview) still say third-party developers need prior approval to offer Claude.ai login or plan rate limits in their products, even when built with the Agent SDK. The legal guide forbids collecting, storing, or intermediating Claude.ai credentials or session tokens. The [Claude login guidance](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account) explicitly includes open-source projects in its third-party discussion and forbids misrepresenting a client's identity or routing third-party traffic against subscription limits.
- Pi's [Messages source](https://github.com/earendil-works/pi/blob/main/packages/ai/src/api/anthropic-messages.ts), reviewed 2026-10-02 on mutable `main`, used `claude-cli/...`, `x-app: cli`, Claude Code/OAuth beta flags, mapped tool names and a Claude Code system instruction for its native OAuth branch. It did not merely launch the unmodified binary. This demonstrates identity shaping, not Arany authorization or subscription billing; those identity claims must not be copied.
- There is a distinct published exception: the [legal guide](https://code.claude.com/docs/en/legal-and-compliance) permits an end user to sign in to the **unmodified Claude Code binary** with their own subscription, including when a product hosts that binary under its stated commercial conditions. Anthropic documents [`claude -p` as a subprocess](https://code.claude.com/docs/en/agent-sdk/overview) for non-Python/TypeScript callers. This is Claude Code running its own agent, not a credential or inference API for Arany's native Provider. Its [headless guide](https://code.claude.com/docs/en/headless) says `--bare` does not read subscription OAuth credentials, while non-bare `-p` can load project hooks and MCP without a trust prompt. The same guide documents automatic API retries and retry events; the [environment reference](https://code.claude.com/docs/en/env-vars) offers `CLAUDE_CODE_MAX_RETRIES` to override the default retry count, but neither source promises that a setting disables every retry or other continuation path. The specifically documented first two silent 401/403 retries apply to `apiKeyHelper`, not subscription OAuth. **Inference:** observing or cancelling only emitted retry events would not establish Arany's zero-automatic-retry invariant; any subprocess integration would first need separate architecture, process-security, and contract review.
- Claude Code also exposes a [mods model-call API](https://code.claude.com/docs/en/plugins/mods/api#call-a-model): `$.model.complete` uses the current Claude Code session's plan or API credentials for a separate prompt, with `maxTokens`, `timeoutMs`, and optional effort. The [mods overview](https://code.claude.com/docs/en/plugins/mods/overview) places handlers inside Claude Code, including `claude -p` and SDK sessions, and warns that mods are not sandboxed. **Inference:** this is an extension of the first-party runtime, not a published standalone OAuth registration or native inference grant for Arany. Bridging it into Arany would need a separate process/extension architecture decision and contract review, not a drop-in replacement for the API-key adapter.
- Anthropic also publishes an [experimental Claude Code routine trigger](https://platform.claude.com/docs/en/api/claude-code/routines-fire): a subscriber creates a per-routine token in Claude Code on the web, and an HTTP POST starts that saved routine on Anthropic-managed infrastructure. It returns only a session ID and URL, without streaming or waiting for model output; the token grants no read access and the endpoint is outside the Messages API. **Inference:** this is an authorized automation trigger for its narrow purpose, not a third-party OAuth login or a native turn-by-turn Provider route for Arany.
- Billing is not safe to infer from the existence of OAuth. The [Claude login guidance](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account) says Anthropic may allow certain paid-subscriber third-party uses at its discretion and may charge usage credits. The [June 2026 Agent SDK billing notice](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan) pauses a proposed credit change and says SDK, `claude -p`, and third-party usage continue drawing from subscription limits for now. These statements cover different eligibility and accounting cases; neither is a blanket authorization or a reliable billing promise for Arany.
- Anthropic's [subscription/API distinction](https://support.claude.com/en/articles/9876003-i-have-a-paid-claude-subscription-pro-max-team-or-enterprise-plans-why-do-i-have-to-pay-separately-to-use-the-claude-api-and-console) says Pro/Max does not include Console/API access. Its [API authentication guide](https://platform.claude.com/docs/en/manage-claude/authentication) documents API keys, workload identity federation, and App Attest, not a general consumer-plan grant for direct Messages API calls. API keys explicitly support `Authorization: Bearer`; the legacy `x-api-key` header used in the [Messages curl example](https://platform.claude.com/docs/en/api/messages/create) remains supported too. A bearer header therefore does not imply subscription OAuth. The guide also requires `anthropic-workspace-id` for personal/service-account keys not scoped to one workspace; scoped keys may omit it. The API-key path remains separately billable, with its own credential/workspace configuration and strict response validation. Arany's native model checks are optional diagnostics; they do not authorize consumer-plan access.
- Anthropic also documents [`ant auth login`](https://platform.claude.com/docs/en/cli-sdks-libraries/cli/authentication): a browser OAuth login **to Claude Console** for Anthropic's `ant` CLI, issuing a workspace-scoped API token for local development and scripting. **Inference:** this is not Claude.ai Pro/Max login, a subscription billing entitlement, or a published grant for Arany to reuse the CLI's credential; the [third-party developer restriction](https://code.claude.com/docs/en/legal-and-compliance) still applies. Its existence does not change the approval gate for a native Claude-plan Provider.

## Rejected authentication shortcuts

The dependency comparison below was checked on 2026-09-29; it is a source review,
not a current vulnerability finding or a native-store result.

- [`openidconnect` 4.0.1](https://docs.rs/openidconnect/4.0.1/openidconnect/#importing-openidconnect-selecting-an-http-client-interface)
  warns that HTTP redirects permit SSRF; its [standards table](https://docs.rs/crate/openidconnect/4.0.1)
  did not support `azp` verification. A library cannot choose endpoint authority
  or the accepted identity profile for Arany.
- [`webbrowser` 1.2.4](https://github.com/amodm/webbrowser-rs/blob/v1.2.4/src/unix.rs)
  selected Linux executables through ambient settings, PATH and desktop config.
  Scheme validation therefore did not establish executable provenance.
- Apple's [Protected Data backend](https://docs.rs/apple-native-keyring-store/1.0.2/apple_native_keyring_store/protected/index.html)
  documents provisioning profiles as required for its full feature set and the iCloud
  capability for the cloud-synchronized store; that is distinct from the unsigned-CLI
  Keychain backend reviewed in [native credential evidence](../security/beta-macos-credential-source-review.md).
  Neither backend selection nor a successful service connection proves writable
  storage or rollback after a timeout; [D-11](./next-step-decision-register.md#d-11--configuration-credentials-and-custom-endpoints)
  retains the uncertainty rationale.

## Decision boundary for beta 1

[Provider admission](../specs/provider.md#provider-support-summary) owns provider
route choices; [D-18](./next-step-decision-register.md#d-18--explicitly-accepted-chatgpt-plan-route-is-beta-scope)
owns acceptance of the weaker ChatGPT-plan guarantee. Native Claude-plan use needs prior Anthropic approval or a
changed published route; the unmodified-binary path requires its own architecture
and security decision. [Next steps](../../NEXT_STEPS.md#beta-2-and-broader-release)
owns live checks. Recheck authorization, billing and availability before use or
public distribution; this prose consolidation advances no retrieval date.
