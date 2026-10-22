# Arany account setup

This guide explains account setup and everyday recovery. The [credential
contract](./specs/credentials.md) owns account scope and storage; the [terminal
contract](./specs/terminal.md) owns interaction. [Provider spec](./specs/provider.md)
and [ChatGPT spec](./specs/chatgpt.md) retain route and OAuth admission.

## Start an attached conversation

1. Run `arany` in the project directory. Review the folder-trust choice before
   account access. Read only or Escape leaves trust unremembered; Ctrl+C exits.
   [Local Tools](./tools.md) explains what trusting a folder enables.
2. Arany reuses your last explicit saved account/model selection. Without a
   remembered selection, coexistence of API and ChatGPT accounts requires an
   explicit billing-route choice; Cancel starts focused. Unavailable access
   never falls back to another account or billing route.
3. For a selected keyring account, the OS may request access to that Arany item
   before chat opens. Arany does not enumerate passwords or unlock the whole
   store. Escape or Ctrl+C cancels startup. On macOS, Allow Once can prompt again
   at later access; the OS owns that choice.
4. If no account is configured, chat still opens. Use `/setup`, or Ctrl+K → Setup,
   to connect one. `arany --setup` opens the wizard at startup. API-key entry hides
   the key; non-secret fields remain visible.
5. Setup reads the selected account's model catalog and chooses a low-effort
   default without inference. Use `/model` to change model and reasoning, then
   submit a task. Submission can consume API or plan usage; catalog visibility
   alone does not prove model compatibility.

A positional task such as `arany "Your task"` requires a Provider/model selection
at startup. If it is missing, configure access in chat first. A fork starts with
unselected defaults; resume retains its saved selection and still revalidates
access. A replaced or disconnected account requires explicit recovery.

## Select a model and keep your draft

`/model` opens the model and reasoning picker. Up/Down selects a row, Left/Right
changes its reasoning, typing filters IDs, Enter saves both and Escape cancels.
Screen-reader mode uses numbered pages and `NUMBER [EFFORT|default]`. Literal
selection is available as `/model MODEL [EFFORT|default]`; use an ID visible to
your account. Unknown native and ChatGPT models offer explicit effort levels
rather than provider default.

Cached lists can open while refreshing. A failed refresh preserves the older
list and shows an error; retry `/model` or review `/setup`. Selecting a model
starts no inference or compatibility check. During a Run, model inspection
shows the pinned selection without changing it.

The composer retains text and caret across selectors, resize and background
catalog work. Enter during a Run or compaction retains the draft without
queueing another task. Ctrl+C cancels active work. Once idle, review the retained
draft and press Enter explicitly. Errors do not automatically retry a task or
restore an already consumed submission.

Use Ctrl+O for a newline, `/paste` for an explicit clipboard read and `/help` for
available commands. The draft is limited to 8 KiB of UTF-8; invalid or oversized
insertions reject without submitting a truncated task. Clipboard PNGs are
limited to four images totaling 192 KiB. Original images remain in private
history and JSONL exports; human history shows metadata. See [paste and
attachments](./specs/terminal.md#paste-and-attachments) for transport, privacy and
model limitations.

Fullscreen, the default chat view, provides managed history with a visible
composer. Classic inserts confirmed turns into native scrollback; choose it with
`--classic` for one process or save it as the chat view in `/settings`. Ctrl+T
or `/transcript` shows Tool details and public commentary. Visual streaming is
provisional: interrupted saved prefixes remain incomplete and
outside later model context. `--screen-reader` uses linear confirmed output.
[Terminal continuity](./specs/terminal.md#terminal-continuity-and-provisional-text)
owns these distinctions and recovery limits.

## Connect or recover a ChatGPT account

Choose ChatGPT plan in `/setup`. For a saved account:

| Choice | Effect |
|---|---|
| Use saved | Reuses a connected account without browser sign-in; multiple accounts open a picker |
| Reconnect | Reauthorizes the selected identity and retains its storage backend |
| Connect new | Starts a separate account connection |
| Enable plan | Appears for a verified identity whose plan permissions are disabled |

Before browser sign-in, review the full warning and explicitly select Accept.
Back starts focused. Local response/time limits do not guarantee a remote
generation or plan-usage cap. Storage has its own warning: an unlocked Linux
Secret Service item may be accessible to another same-user app; the private-file
backend stores credentials without encryption. Resize or resume restarts review.

Reconnect requires the same signed account identity. A browser success page is
not proof of completed sign-in: Arany must still validate the issuer, signature,
audience, nonce and time claims. Follow the safe error category; do not bypass
validation or paste tokens, callback URLs or raw token responses into a report.
If a prior new sign-in left a registration, Resume sign-in reuses it with a fresh
authorization attempt; Back preserves it without connecting an account.

Cancelling before account publication preserves the existing selection. After a
new connection, switch or reconnect has saved the account, cancelling or failing
catalog loading can leave it selected without model/effort defaults. Use `/model`
to finish selection. Cancellation does not roll back a saved account or a token
refresh already started.

If plan permission is disabled, `/setup` → Enable plan explicitly requests it for
the same registration and identity. No catalog, check or Run can use the disabled
account. Arany never switches to API billing automatically. Anthropic consumer
subscriptions remain unsupported; Anthropic API keys use separate billing.

To disconnect the selected ChatGPT account:

```sh
arany provider logout chatgpt
```

Logout blocks account use, attempts remote revocation and removes the local
token while retaining registration for Reconnect. Follow any warning about
unconfirmed revocation or token deletion. It does not erase backups or cancel
requests already in flight. [ChatGPT spec](./specs/chatgpt.md) owns the precise
renewal, permission and logout behavior.

## Storage and recovery

Accounts belong to the effective OS user across Sessions. `--state-dir` changes
Session/Tool configuration storage, not saved-account scope. Linux uses Secret
Service; macOS uses Keychain Services. A locked or pinned keyring failure never
silently changes to a private file. For a new unpinned account, an unavailable
store can offer Use private file with explicit plaintext consent; Cancel starts
focused. Reconnect retains its pinned backend.

Private permissions do not encrypt files or exclude another same-user or
privileged process. Session history is also unencrypted. OS-store authorization
has a bounded interactive wait; routine store operations have shorter deadlines.
An uncertain write or deletion is not a confirmed rollback. Unlock the selected
store and retry deliberately, or follow the explicit new-account storage choice.

Malformed or unsupported saved account data keeps access unconfigured and
provides `/setup` recovery. A confirmed local reset removes only the disclosed,
checked records for the selected route and saved model choices. It preserves
conversations, folder trust and other-route accounts, but does not revoke remote
access or remove old password-store entries. Back cancels; cancelling subsequent
setup cannot restore records already reset.

History recovery is separate. Unsupported or damaged history offers Retry,
Reset history and Exit. Reset requires explicit warning confirmation and discards
conversations and derived Provider evidence in that StateRoot, preserving
accounts, model choices and folder trust. Unsafe files or active/concurrent use
refuse reset. No reset migrates unsupported records or automatically resubmits an
objective. See [credential recovery](./specs/credentials.md#storage-replacement-and-recovery)
and [history recovery](./specs/terminal.md#setup-and-account-choices).

## Headless API-key use

Provide the selected key through `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`, without
putting it in command arguments. Native `exec` does not prompt or implicitly read
the saved default API account. List models, then replace `MODEL_ID` with a chosen
ID and select supported effort:

```sh
arany provider models openai
arany exec --provider openai --model MODEL_ID --effort low "Your task"
```

For a saved native catalog, use `arany provider models openai --saved-account`.
Inside chat, `/model` uses the Session's pinned account. Account replacement can
make an older Session's saved identity unavailable; it never silently substitutes
the new credential.

An Anthropic API key not limited to one Console workspace needs an explicit
`ANTHROPIC_WORKSPACE_ID` alongside the environment key. A workspace-scoped key
may omit it. Saved-account setup instead asks Key scoped to workspace or Choose
API workspace; saved selections never inherit ambient workspace variables.
[Provider account spec](./specs/provider.md#native-models-and-accounts)
own workspace validation and admission.

## Anthropic-compatible custom routes

Generic Messages routes require a private version-2 `provider-profiles.json` in
`STATE` and exact conformance before use. Only the selected `ARANY_PROVIDER_*`
environment key is used; native Anthropic accounts and workspace headers do not
supply credentials. Keep the file private (`0600` on Unix).

**DeepSeek is UNAVAILABLE until strict compaction is proven.** It lacks evidence
for the provider-enforced compaction schema Arany requires. Ordinary answers,
delegation and plausible summary JSON cannot establish that support. Do not
configure DeepSeek or relax compaction to enable it.

Kimi platform API keys use the following complete document in
`STATE/provider-profiles.json`, outside the Workspace, with mode `0600`:

```json
{
  "version": 2,
  "profiles": [
    {
      "name": "kimi-api",
      "protocol": "anthropic-messages",
      "base_url": "https://api.moonshot.ai/anthropic",
      "auth": "bearer",
      "model": "kimi-k3",
      "credential_env": "ARANY_PROVIDER_KIMI_API_KEY",
      "outcome_encoding": "native_tools",
      "privacy": "user_authorized",
      "max_output_tokens": 4096,
      "capability_evidence_version": 2,
      "efforts": []
    }
  ]
}
```

Supply your platform key through `ARANY_PROVIDER_KIMI_API_KEY`, using your
private environment outside the repository. The platform documents
schema-constrained output, but the exact endpoint/model must still pass Arany's
unchanged conformance check. Checks send synthetic data only and may be billable:

```sh
arany provider check custom:kimi-api --state-dir STATE
```

Only after that exact check succeeds:

```sh
arany exec --state-dir STATE --provider custom:kimi-api --model kimi-k3 "Your task"
```

An empty effort list omits the override. Explicit platform efforts
`["low", "high", "max"]` require individual conformance; never inherit Claude
defaults or translate unsupported values. The platform base sends to
`https://api.moonshot.ai/anthropic/v1/messages`.

**Kimi Code is UNAVAILABLE pending endpoint-specific strict-compaction evidence.**
Its console keys belong to a separate subscription service and billing route.
Platform schema documentation does not establish Code support; this is unknown,
not proof that Code cannot enforce schemas. Keep this candidate separate from
the usable platform document; it grants no admission:

```json
{
  "version": 2,
  "profiles": [
    {
      "name": "kimi-code",
      "protocol": "anthropic-messages",
      "base_url": "https://api.kimi.com/coding/",
      "auth": "x-api-key",
      "model": "k3-256k",
      "credential_env": "ARANY_PROVIDER_KIMI_CODE_KEY",
      "outcome_encoding": "native_tools",
      "privacy": "user_authorized",
      "max_output_tokens": 4096,
      "capability_evidence_version": 2,
      "efforts": []
    }
  ]
}
```

The Code candidate sends to `https://api.kimi.com/coding/v1/messages`; retain
`.com` unless the owner explicitly selects another region. Confirm the exact
tier-visible model and billing settings first. Other Code candidates (`k3`,
`kimi-for-coding`, `kimi-for-coding-highspeed`) each need their own exact check;
a mistyped HighSpeed ID can select another model. An entitlement failure can
return 401 and never selects a platform key or another billing route. Native
Anthropic credentials and the two Kimi keys are isolated, even when another key
is available. Arany always identifies itself as `arany/<version>`; it never
impersonates kimi-cli or reuses its OAuth client or registration.

The [dated vendor research](./research/deepseek-kimi-glm-providers-2026-10.md#kimi-platform-api-and-code-subscription)
links the platform schema and Code endpoint, model and identity sources. Existing
valid version-1 Responses profiles migrate automatically and require a fresh
check. Live acceptance and native platform evidence remain
[owner checks](../NEXT_STEPS.md#daily-use-first).

**GLM Standard API is UNAVAILABLE pending Messages strict-compaction evidence.**
Z.ai's [structured-output guide](https://docs.z.ai/guides/capabilities/struct-output)
documents Chat Completions JSON mode, prompt-defined structure and client-side
schema validation; it does not establish provider-enforced Messages JSON schema.
Keep these candidates separate from the usable Kimi platform recipe; neither a
synthetic pass nor plausible summary JSON establishes vendor support.

Candidate document for `STATE/provider-profiles.json`, outside the Workspace,
with mode `0600`:

```json
{
  "version": 2,
  "profiles": [
    {
      "name": "glm-api",
      "protocol": "anthropic-messages",
      "base_url": "https://api.z.ai/api/anthropic",
      "auth": "bearer",
      "model": "glm-5.3",
      "credential_env": "ARANY_PROVIDER_GLM_STANDARD_KEY",
      "outcome_encoding": "native_tools",
      "privacy": "user_authorized",
      "max_output_tokens": 4096,
      "capability_evidence_version": 2,
      "efforts": []
    },
    {
      "name": "glm-flash",
      "protocol": "anthropic-messages",
      "base_url": "https://api.z.ai/api/anthropic",
      "auth": "bearer",
      "model": "glm-5.3-flash",
      "credential_env": "ARANY_PROVIDER_GLM_STANDARD_KEY",
      "outcome_encoding": "native_tools",
      "privacy": "user_authorized",
      "max_output_tokens": 4096,
      "capability_evidence_version": 2,
      "efforts": []
    }
  ]
}
```

Both candidates derive `https://api.z.ai/api/anthropic/v1/messages`. Use only a
newly issued, console-classified **Standard API Key**, supplied privately as
`ARANY_PROVIDER_GLM_STANDARD_KEY`; confirm account-balance or standard-bundle
billing first. Reject pre-update **Compatible** keys: they can consume Coding
Plan quota at this same endpoint. Key type cannot be inferred from opaque key
bytes or a successful check; the owner must verify its console classification.
The [key-management guide](https://docs.z.ai/devpack/notice/api-key-management.md)
explains the October 16, 2026 billing split.

The **GLM Coding Plan remains EXCLUDED**: Arany is absent from its
[supported tools](https://docs.z.ai/devpack/tool/others), and its
[terms, section 4](https://docs.z.ai/legal-agreement/subscription-terms) restrict
subscription quota to approved tools. Never substitute a plan key, Compatible
key, ZCode OAuth identity or another client's User-Agent. Arany sends its honest
`arany/<version>` identity and never falls back to native or Kimi credentials.

Keep `efforts: []` to omit native Claude overrides. Chat `reasoning_content`
and JSON mode do not establish Messages thinking or schema support. Before
promoting either candidate, obtain Messages-specific provider-enforced schema
evidence, then run the exact `arany provider check custom:glm-api` or
`custom:glm-flash` with `--state-dir STATE`. These checks use synthetic data,
may be billable and must retain the schema-only nonce challenge. Each model
needs its own passing check before a Run; failure leaves it unadmitted.
[Vendor research](./research/deepseek-kimi-glm-providers-2026-10.md#glm-standard-api-key-subscription-excluded)
owns the source review; [owner checks](../NEXT_STEPS.md#daily-use-first) own live
billing, compatibility and native evidence.

## Select a private roster preset

Create a private `provider-roster.json` in the State directory outside the Workspace, using owner-only file permissions:

```json
{"version":1,"profiles":[{"name":"main","routes":[{"provider":"openai","model":"gpt-6.1-sol","effort":"medium","account_id":null}]}],"presets":[{"name":"coding","providers":{"primary":"main","child":null,"review":null,"compaction":null},"child_profiles":["main"]}]}
```

With the selected route's existing credentials available, use `arany --profile coding` or `arany exec --profile coding "Inspect this project"`. Review the complete billing/route manifest; all candidates must admit before project input. Remove any competing `provider-routing.json` source explicitly. The [Provider contract](./specs/provider.md#named-roster-presets) owns the format, bounds and current fixed-role delivery boundary; this example makes no live-model support claim.

## Headless ChatGPT use and optional diagnostics

After consenting to and connecting the intended account, list its models and
select one explicitly:

```sh
arany provider models chatgpt
arany exec --provider chatgpt --model MODEL_ID --effort high "Your task"
```

These commands use only the selected consented OS-user ChatGPT account; they
never fall back to `OPENAI_API_KEY`. A catalog read may refresh a near-expiry
token. A Run revalidates account identity and consent before Workspace input.

Compatibility checks are optional for ordinary native/ChatGPT Runs and can
consume usage. Only after choosing the account/model and accepting that cost:

```sh
arany provider check chatgpt MODEL_ID --effort high --accept-cost
arany provider check openai MODEL_ID --effort high --accept-cost
```

The native command uses the environment key; `--saved-account` explicitly selects
the saved native account instead. ChatGPT always uses its selected consented
account. Each check makes at most three synthetic calls and sends no Workspace
content. ChatGPT's output cap is local acceptance only; failure or cancellation
does not prove remote usage stopped. Checks do not authorize fallback, retries or
broader disclosure. Exact custom profiles have their own mandatory conformance
admission under [Provider spec](./specs/provider.md).

## Resume and inspect results

Use `/resume` to choose a saved conversation, `/rename TITLE` to name it, and
`/new` or `/clear` to begin an empty one with the current selection. Older
conversations remain saved. `/compact` requests a separate context-summary call;
it can consume usage and does not delete canonical history. Failed or cancelled
maintenance preserves the prior accepted summary.

```sh
arany show --output text SESSION_ID
arany show --output jsonl RUN_ID
```

Replace the placeholders with saved IDs. Inspection does not invoke a Provider
or change history; JSONL can contain original image bytes and private task data.
After uncertainty, inspect saved facts and actual files before an intentional
retry. Development builds expose bounded safe failure categories in the
[local diagnostic log](./development-diagnostics.md).

[Next steps](../NEXT_STEPS.md) owns pending native, live and accessibility checks.
Developers running an explicitly authorized live gate should follow the
[native/live evidence procedure](../agents/references/testing-details.md#native-and-live-evidence)
and its test owner. This guide does not establish support for an untested model,
account, OS or credential backend.

## Use an ACP editor

Configure your editor's ACP agent with the absolute path to `arany` and arguments
like these (the editor supplies piped stdin/stdout):

```text
acp --provider openai --model gpt-5.4
```

Use your admitted model/effort tuple, or `acp --profile NAME` for a private
roster. `arany --setup` configures accounts in a terminal; ACP's authentication
method only rechecks existing access and never accepts a key. Explicit native
API flags use environment credentials, as `exec` does; a roster can select a
saved account. A GUI-launched macOS editor may not inherit shell environment
keys, so configure the editor launch environment or use an admitted saved
account roster. Do not put credentials in ACP messages.

The [ACP contract](./specs/acp.md) lists the supported core text surface. Tools
and rich attachments are unavailable in this slice; ignored editor MCP entries
produce one stderr notice. Native editor compatibility remains a user-owned
check in NEXT_STEPS.
