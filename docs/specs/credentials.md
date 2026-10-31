# Saved credentials

## Scope and related contracts

This contract owns saved-account scope, protected storage, selected-item startup authorization, replacement and recovery. Behavior is shared across OSes; native storage mechanisms and evidence can differ ([next steps](../../NEXT_STEPS.md#beta-2-and-broader-release)). Provider admission belongs to the [Provider contract](./provider/README.md); OAuth, verified identity, consent and token renewal to the [ChatGPT contract](./chatgpt.md); consent ordering to [folder trust](./tools/trust-and-approvals.md#folder-trust-and-approvals); account choices, interactive cancellation and account inspection to the [terminal contract](./terminal/setup-and-accounts.md#setup-and-account-choices). Editing constraints: [security rules](../../agents/security.md).

Session State keeps Tool output outside the Workspace in private `0700` directories with `0600` files; the [output spill contract](./tools/limits-and-failures.md#limits-and-failure-behavior) owns layout and retention.

## Account scope and identity

A saved Provider Account belongs to one effective OS user across that user's Sessions and StateRoots, never the whole machine. Account metadata and its replacement lock live under the user's passwd home, independent of `HOME`, XDG variables and `--state-dir`: `.local/state/arany` on Linux, `Library/Application Support/dev.Arany.arany` on macOS. Keyring items stay in that user's OS credential store. Environment-selected Session roots never supply or receive account records.

Private account and Session directories must be physical, owner-private (`0700`) paths with no symlinked ancestor, so macOS `/tmp` and `/var` aliases reject; root-owned sticky temporary ancestors are admissible. Links, non-regular objects (a FIFO included, without blocking), unexpected owners or ACLs, predictable shared temporary paths and state inside an untrusted Workspace reject. Opening creates no records or locks. Test-only root overrides work in development or explicitly instrumented test builds; ordinary optimized binaries reject them before any OS-store access.

Native API-account records validate, on read and write, the exact OpenAI or Anthropic Provider, UUIDv7 identity, bounded printable key, and either a reviewed model and effort or a bounded model ID with explicit effort, plus an explicit nullable workspace scope that, when set, must be a validated Anthropic API workspace. Unsupported versions, missing fields, invalid scope and cross-Provider scope reject. Saved scope is never reconstructed from environment variables.

A Session stores the saved account UUID, never its credential. Native Run and catalog use resolve that UUID and Provider against the current protected record, so a replaced account is unavailable to an older Session instead of switching billing identity. Environment-backed and saved access never fall through to each other. A saved model and effort is not compatibility evidence: Run admission and strict validation still apply.

Custom routes read only their selected `ARANY_PROVIDER_*` environment reference after destination admission; saved accounts never supply them. Kimi Code console keys use `ARANY_PROVIDER_KIMI_CODE_KEY`, distinct from Kimi platform keys (`ARANY_PROVIDER_KIMI_API_KEY`) and native Anthropic credentials. The GLM candidate uses only `ARANY_PROVIDER_GLM_STANDARD_KEY`, a newly issued Standard key whose console classification and standard billing the owner confirms; Compatible and Coding Plan keys are excluded. These keys create no saved account or OAuth flow. A missing selected key, a 401, a redirect or model drift never selects another key, vendor, billing route or native account. The [custom destination contract](./provider/routing-and-usage.md#custom-destinations-and-routing) owns authentication headers and scope.

Rosters may use several models or efforts on one admitted identity; any unsupported combination of saved native or ChatGPT accounts, including an unused candidate, refuses before Workspace content. Rosters never enumerate credential slots or expand the account store. [Provider presets](./provider/selection.md#named-roster-presets) own route composition and candidate ordering.

## Metadata-only storage inspection

The explicit local storage report reads only aggregate sizes and counts under the account root, never record names, contents, selected credentials, the account lock or the OS keyring. It authorizes no model read, account selection, credential access, repair or creation; missing roots stay absent and malformed payloads do not prevent it. Reports never enter model input, Events, logs, telemetry or MCP. The [storage inventory contract](./tools/commands-and-cache.md#local-storage-inventory) owns coverage, bounds and refusal.

## Backend selection and startup

Setup prefers the OS keyring. An unavailable, unpinned store may offer a separately warned, explicitly selected private-file backend; a locked store reports a locked error. A backend marker pins the choice; read, write or authorization failure on a pinned keyring never downgrades storage.

Bare startup reuses the last explicit saved-source preference without loading credentials; disconnected ChatGPT metadata cannot restore access. Without a preference, coexisting API and ChatGPT records need an explicit billing-route choice; cancellation reads neither payload and creates no Session. An invalid selected record never falls back to the other route. A native keyring item without a current backend marker is never loaded as a configured account; explicit native setup admits it.

On Linux, Secret Service connects only to the session bus named by a bounded address or `XDG_RUNTIME_DIR`, without autolaunch.

### Selected-item authorization

After folder consent and account choice, attached startup authorizes only the selected keyring-backed Arany item, before Provider catalog, preflight or chat input, never enumerating, unlocking the whole store or changing ACLs. The check returns no credential. Unconfigured, environment-backed, custom and private-file selections skip OS-store access. Native-account discovery uses the same cancellable wait and keeps no credential cache. Neither path refreshes tokens, starts sign-in, grants Run authority or calls a Provider; the account is revalidated and its credentials reloaded at use.

Interactive startup access has a two-minute human-interaction deadline; cancellation restores the terminal. Denial, missing or invalid items, identity replacement and timeout give safe startup feedback and submit no objective, with no Provider call, sign-in, automatic retry or fallback. Routine reads, writes and deletion have five-second deadlines. On macOS the OS owns per-item permission persistence; Allow Once may prompt again.

## Storage, replacement and recovery

An incompatible or malformed saved account or model preference never prevents opening the attached CLI: Arany shows recovery guidance and keeps access unconfigured until the user selects valid access, never switching billing routes, loading another credential or submitting an objective automatically. A recoverable failure during initial `--setup` returns to the composer. Headless commands reject with attached-setup guidance.

Setup offers an explicit local reset for invalid selected-route data, Back focused. Confirmed reset discards only that route's account records, its ChatGPT registration when applicable, and saved model choices; conversations, folder trust and other-route accounts stay. A concurrent change refuses reset, cancellation writes nothing, and unsafe or over-limit objects cannot be reset this way. Invalid saved model choices can be reset separately. A confirmed reset is a completed local effect even if setup is later cancelled. Reset never interprets an unsupported format, enumerates the password store or revokes remote access, and says old password-store entries may remain.

A partial storage failure is [Uncertain](./execution/events-and-outcomes.md#effect-outcomes), claims no rollback and blocks setup continuation. Once an OS-store write or deletion is dispatched, an unconfirmed result is Uncertain, even on a first-time save; a locked-store response is a confirmed refusal. The backend stays pinned. Uncertain deletion keeps account use blocked and warns the user. After a recognized terminal refresh-token error the account is disconnected before deletion; failed cleanup never re-enables use and stays visible. ChatGPT logout deletes only its own token slot, never the native API item.

The private-file backend is plaintext at rest beneath a user-owned `0700` root, with `0600` single-link records; on macOS, extended ACL allow entries reject and more than 128 entries fail closed. Writes are atomic; a crash may leave one bounded pending file. Permission modes do not encrypt credentials, exclude same-UID or privileged processes or securely erase old disk contents.

Native Keychain access uses only the exact user-domain Keychain, service and slot, never changing the default, search list or item permissions. Replacement inserts only when the selected item is absent; locked or denied access never becomes a new item or plaintext fallback.

A native API account's Provider is the closed `{kind, custom_name}` record: native kinds require a null custom name, and custom and ChatGPT kinds reject. The account file, its backend marker, the ChatGPT index and registration are [strict records](./execution/persistence.md#incompatible-records) without a version: one the current decoder refuses is archived intact at startup and setup starts fresh. A refused keyring record is never moved or rewritten; the user resets setup or signs in again. ChatGPT registration carries only its host ID. Native replacement pins the inspected account and credential digest and refuses any change before it commits.

Native-account removal confirms a snapshot of the account, model preferences and keyring identity, refuses if it went stale, and removes the marker and preferences before deleting only the unchanged keyring slot, so uncertain deletion leaves the credential unconfigured; an orphaned keyring item is never loaded without its marker. ChatGPT removal in setup is local only ([logout](./chatgpt.md#logout)). Secret key and workspace-ID entry restore echo on cancellation.

### Remote MCP tokens

`arany mcp login <server>` saves one OAuth grant per configured [remote MCP server](./tools/skills-and-mcp.md#remote-mcp-and-oauth) in `mcp-oauth.json` in the account root: at most four servers, each with its URL, issued client ID, token and optional revocation endpoints, access token, optional refresh token and optional expiry, in a record of at most 64 KiB<!-- unrelated-bound: MCP OAuth record --> stored as a version-1 [versioned record](./execution/persistence.md#incompatible-records). The grant is an unencrypted private file with no keyring slot, readable by any process of the same user, which login says before printing the authorization URL. Tokens never enter Events, logs, telemetry or Tool observations, and a grant is sent only to the exact URL it was issued for. Unknown fields, non-HTTPS endpoints and oversized or non-printable tokens reject the whole record; an unreadable record means no token. Logout deletes the entry, and the file with its last entry, even when remote revocation is unconfirmed. Concurrent writers never lose another's entry. Each grant carries `renewal` (`ready`, `renewing`, `rejected`); the [renewal rules](./tools/skills-and-mcp.md#remote-mcp-and-oauth) say how an unknown refresh outcome blocks reuse.

## Limits and record admission

Native API-account records, their backend marker and private-file records are each at most 1 KiB<!-- unrelated-bound: native account metadata records -->. A ChatGPT host record is 256 bytes. `chatgpt-accounts.json` is at most 384 KiB and holds at most eight accounts sharing one host ID; keyring entries hold metadata only, private-file entries the plaintext token pair. `account-credentials.lock` is a zero-byte file beneath the account root. The ChatGPT keyring token record is at most 64 KiB<!-- unrelated-bound: ChatGPT helper token record -->. Invalid or oversized records reject without partial admission.

Credentials never pass through process arguments or the environment and never enter Events, logs, telemetry or drawing calls. Errors never expose platform error text or secret records. Availability probes return no secret, and a successful probe does not prove later writes will succeed.

Linux targets Secret Service without a KDE or GNOME requirement. OS-store protection does not mean every backend encrypts at rest or restricts an unlocked item to Arany; the [dependency source evidence](../security/beta-dependency-inventory.md#build-time-and-native-authority) records the observed Linux boundary. The Linux keyring consent warning states that another same-user application may read an unlocked item; it claims no Arany-only secrecy and offers no silent plaintext fallback.
