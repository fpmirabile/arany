# Saved credentials

## Scope and related contracts

This contract owns saved-account scope, protected storage, selected-item startup authorization, replacement and recovery. Behavior is shared across OSes; native storage mechanisms and support evidence can differ. [Credential rules](../../agents/credentials.md) own helper isolation, transport admission and fixture safety. [Provider rules](../../agents/provider.md) retain model and billing-route admission; [ChatGPT rules](../../agents/chatgpt.md) retain OAuth, verified identity, account consent and token renewal. [The terminal contract](./terminal.md#setup-and-account-choices) owns folder-consent ordering, account choices and interactive cancellation; [Store rules](../../agents/store.md) own private filesystem admission and atomic persistence.

## Account scope and identity

A saved Provider Account belongs to one effective OS user across that user's Sessions and StateRoots, never the whole machine. On Linux and macOS, account metadata and its replacement lock use the effective user's passwd home, independently of `HOME`, XDG variables and Session `--state-dir`. The native roots are `.local/state/arany` on Linux and `Library/Application Support/dev.Arany.arany` on macOS. Keyring items remain in that user's OS credential store. Session-root configuration retains its separate contract.

Native API-account records validate the exact OpenAI or Anthropic Provider, UUIDv7 account identity, bounded printable key, and either a reviewed model/effort or a bounded model ID with explicit effort on both read and write. Key-only records use schema 1 and omit workspace scope. Schema 2 requires a validated explicit Anthropic API workspace. Unknown or mismatched versions, absent/invalid scope and cross-Provider scope reject. Older binaries reject scoped records rather than discard billing scope; saved scope is never reconstructed from ambient environment variables.

A Session stores the saved account UUID, without its credential. Before native Run or catalog use, resolve that UUID and Provider against the current protected record. Replacing an account makes it unavailable to an older Session. Explicit environment-backed access never falls through to saved credentials, and saved access never falls through to environment credentials. A saved model/effort tuple is not compatibility evidence: actual Run admission and strict request/outcome validation remain required, without a mandatory synthetic check.

## Backend selection and startup

Setup prefers the OS keyring. An unavailable, unpinned store may offer a separately warned, explicitly selected private-file backend. A locked store reports a locked error. The checked backend marker pins keyring storage without a key, or file storage with the account. Read, write or authorization failure on a pinned keyring never silently downgrades storage.

Bare startup first reuses the last explicit saved-source preference, selecting Provider/account/model metadata without loading credentials; disconnected ChatGPT metadata cannot restore access. Without a preference, checked record presence determines available saved routes before either payload is read. When API and ChatGPT records coexist, billing-route choice is explicit. Cancellation reads neither payload and creates no Session; an invalid selected record never falls back to the other route. With an existing ChatGPT index, discovery of an old unpinned native keyring item requires explicit native inspection/setup.

### Selected-item authorization

After folder consent and account choice, attached startup authorizes only the selected keyring-backed Arany item, before Provider catalog/preflight or chat input. The fixed service and validated slot exclude enumeration, whole-store unlock and ACL changes. The status-only operation checks native UUID/Provider identity or the selected ChatGPT slot and returns no credential. Unconfigured, environment-backed, custom and private-file selections skip OS-store access.

Native-account discovery without a remembered preference uses the same cancellable interactive wait for its existing bounded account read, then discards the key before chat without a duplicate authorization or credential cache. Neither path refreshes tokens, starts sign-in, grants Run authority or invokes inference. Revalidate the selected account after authorization and reload current credentials at use.

Interactive startup access has a supervised two-minute human-interaction deadline. Cancellation kills and reaps the helper and restores the terminal according to its contract. Denial, missing/invalid items, identity replacement and timeout report safe startup feedback without a Provider call, sign-in, automatic retry or fallback. Routine reads, writes and deletion retain five-second deadlines. On macOS the OS owns per-item permission persistence; Allow Once may require another prompt on later access.

## Storage, replacement and recovery

The explicitly approved private-file backend is plaintext at rest beneath a checked, user-owned `0700` root. Each fixed record and pending replacement is bounded, no-follow, single-link, owner-checked and `0600`. Atomic rename with file and directory synchronization prevents a partially written final record; a crash may leave one bounded pending file. Permission modes do not encrypt credentials, exclude same-UID/privileged processes or securely erase old disk contents.

On Linux and macOS, a validated native API record in the previous environment-selected root migrates under the old-root lock followed by the stable-root lock. Make the new copy durable before replacing the old record with a version-zero tombstone. An identical partial copy can finish on retry; a different current account reports a redacted conflict without overwriting either record. ChatGPT registration and matching account-index preservation retain their [migration contract](../../agents/chatgpt.md); relocation cannot grant or renew account consent.

Timed-out OS-store writes report an uncertain outcome distinct from a timed-out read. Keep the backend pinned, and do not claim rollback merely because the helper was killed. ChatGPT logout deletes only its validated token slot, never the native API item. An uncertain deletion keeps account use blocked and warns the user. After a recognized terminal refresh-token error, disconnect the account and clear model evidence before attempting deletion; failed cleanup never re-enables use and remains visible.

## Limits and compatibility

Native API-account records and their backend marker retain the aggregate 1-KiB limit. The separate ChatGPT helper slot admits a versioned, consent-bound token record of at most 64 KiB. Invalid or oversized records reject without partial admission. Pass credentials to or from OS-store helpers only through bounded private pipes, never arguments or process-global environment. Keys and tokens never enter Events, logs, telemetry or drawing calls. Errors are redacted and never expose platform error text or secret records. Availability probes return no existing secret.

A successful read-only store probe does not prove later writes will succeed. Linux targets Secret Service without a KDE/GNOME requirement. OS-store protection does not imply that every backend encrypts at rest or restricts an unlocked item to Arany; [the Linux client-isolation finding](../security/findings/linux-secret-service-client-isolation.md) owns that limitation. Native availability, replacement, unlock/failure behavior, private-file ACL admission and per-item authorization need platform-specific evidence before a support claim.

## Acceptance scenarios and evidence owners

These owners define verification scope, not a claim that every native gate passed. [Dated macOS evidence](../research/testing-strategy-for-rust-cli-harness.md#native-macos-baseline-2026-10-08), [the account-scope finding](../security/findings/default-account-slot-cross-root.md), [the stalled-store finding](../security/findings/stalled-credential-store.md) and [next steps](../../NEXT_STEPS.md#beta-2-and-broader-release) retain results and outstanding checks.

| Scenario | Required outcome | Existing evidence owner |
|---|---|---|
| OS-user scope | Changing Session roots, `HOME` or XDG variables does not change the effective user's account root; different OS users retain separate account state. | [Account-path owner](../../tests/session_run.rs), [cross-user finding](../security/findings/default-account-slot-cross-root.md) |
| Selected identity and billing scope | Valid saved records retain identity and explicit workspace; replacement, schema mismatch and ambient scope substitution reject before disclosure. | [Credential corpus](../../src/cli/credentials.rs), [scoped API journey](../../tests/session_run/setup/offline_https/anthropic.rs) |
| Backend pinning | Explicit file consent succeeds when admitted; locked/pinned keyring failure never selects file or environment credentials. | [Credential corpus](../../src/cli/credentials.rs), [setup journey](../../tests/session_run/setup.rs) |
| Selected startup item | Folder consent precedes selected-item authorization, which precedes catalog/chat; unrelated sources launch no credential helper and account changes reject. | [Attached startup composition](../../src/cli/attached/run.rs), [helper protocol corpus](../../src/cli/credentials/keyring_helper.rs) |
| Slow authorization and cancellation | Human interaction can exceed five seconds within the two-minute bound; interruption restores the terminal, kills/reaps the helper and submits no task. | [Supervisor corpus](../../src/cli/credentials/keyring_helper.rs), [attached startup composition](../../src/cli/attached/run.rs); native OS dialog remains a separate gate |
| Failed initial or resumed access | No automatic objective submission, Provider Run, alternate route or replay mutation follows failed startup authorization. | [Attached startup composition](../../src/cli/attached/run.rs), [Linux stalled-store journey](../../tests/session_run/setup.rs) |
| Durable relocation | Valid API records and matching ChatGPT registration/index preserve saved identity; interrupted copies retry, conflicts and unsafe records reject. | [Credential corpus](../../src/cli/credentials.rs), [registration migration corpus](../../src/cli/chatgpt/registration.rs), [private-record owner](../../src/store/state.rs) |
| Routine timeout and uncertain effects | Stalled helpers are killed/reaped within routine bounds; uncertain writes/deletion cannot claim rollback or restore blocked access. | [Helper corpus](../../src/cli/credentials/keyring_helper.rs), [stalled-store native gate](../security/findings/stalled-credential-store.md) |
| Real native store lifecycle | Isolated synthetic items prove save/read/replacement, item authorization and failure/deletion without accessing the user's live account store. | [Native gates](../../NEXT_STEPS.md#beta-2-and-broader-release), [setup guide](../setup.md) |
