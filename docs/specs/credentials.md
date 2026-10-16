# Saved credentials

## Scope and related contracts

This contract owns saved-account scope, protected storage, selected-item startup authorization, replacement and recovery. Behavior is shared across OSes; native storage mechanisms and support evidence can differ. Provider admission belongs to the [Provider contract](./provider.md); OAuth, verified identity, consent and token renewal to the [ChatGPT contract](./chatgpt.md). [The terminal contract](./terminal.md#setup-and-account-choices) owns folder-consent ordering, account choices and interactive cancellation. Editing constraints: [security rules](../../agents/security.md).

## Account scope and identity

A saved Provider Account belongs to one effective OS user across that user's Sessions and StateRoots, never the whole machine. On Linux and macOS, account metadata and its replacement lock use the effective user's passwd home, independently of `HOME`, XDG variables and Session `--state-dir`. The native roots are `.local/state/arany` on Linux and `Library/Application Support/dev.Arany.arany` on macOS. Keyring items remain in that user's OS credential store. Session-root configuration retains its separate contract.

Private account and Session directories require physical paths with no symlinked
ancestor. macOS `/tmp` and `/var` aliases therefore reject; native temporary
fixtures use `/private/tmp`. Admission does not canonicalize supplied paths or
redirect an account to an environment-selected home.

Native API-account records validate the exact OpenAI or Anthropic Provider, UUIDv7 account identity, bounded printable key, and either a reviewed model/effort or a bounded model ID with explicit effort on both read and write. All native API records use schema 2 and explicit nullable workspace scope. A non-null scope requires a validated Anthropic API workspace. Unsupported versions, missing fields, invalid non-null scope and cross-Provider scope reject. Saved scope is never reconstructed from ambient environment variables.

A Session stores the saved account UUID, without its credential. Before native Run or catalog use, resolve that UUID and Provider against the current protected record. Replacing an account makes it unavailable to an older Session. Explicit environment-backed access never falls through to saved credentials, and saved access never falls through to environment credentials. A saved model/effort tuple is not compatibility evidence: actual Run admission and strict request/outcome validation remain required, without a mandatory synthetic check.

Admission walks each path component through a no-follow directory handle. New components are created `0700`; existing leaves need the current owner and private permissions. Root-owned sticky temporary ancestors are admissible, and symlinked ancestors are not. Opening creates no records or locks. Private leaves, including State records, are opened nonblocking before their type check, so a FIFO fails admission instead of blocking.

Debug builds only accept `ARANY_TEST_ACCOUNT_ROOT`, which must be an absolute path with explicit `HOME` and XDG settings beneath a non-root fixture parent. Optimized builds reject it, and Store admission rejects an environment-selected state root outside that parent. The macOS Keychain fixture likewise accepts `ARANY_TEST_KEYCHAIN_ROOT` only in debug builds, to select a fixed file inside an admitted private root; optimized builds reject it before any OS-store access.

## Backend selection and startup

Setup prefers the OS keyring. An unavailable, unpinned store may offer a separately warned, explicitly selected private-file backend. A locked store reports a locked error. The checked backend marker pins keyring storage without a key, or file storage with the account. Read, write or authorization failure on a pinned keyring never silently downgrades storage.

Bare startup first reuses the last explicit saved-source preference, selecting Provider/account/model metadata without loading credentials; disconnected ChatGPT metadata cannot restore access. Without a preference, checked record presence determines available saved routes before either payload is read. When API and ChatGPT records coexist, billing-route choice is explicit. Cancellation reads neither payload and creates no Session; an invalid selected record never falls back to the other route. A native keyring item without a current backend marker is not a configured account and is never loaded during discovery.

On Linux, Secret Service admission accepts one bounded `unix:path=` or `unix:abstract=` address with an optional exact GUID, or the bounded absolute `XDG_RUNTIME_DIR`. With both absent, the client connects directly to `/run/user/<uid>/bus` without autolaunch.

### Selected-item authorization

After folder consent and account choice, attached startup authorizes only the selected keyring-backed Arany item, before Provider catalog/preflight or chat input. The fixed service and validated slot exclude enumeration, whole-store unlock and ACL changes. The status-only operation checks native UUID/Provider identity or the selected ChatGPT slot and returns no credential. Unconfigured, environment-backed, custom and private-file selections skip OS-store access.

Native-account discovery without a remembered preference uses the same cancellable interactive wait for its existing bounded account read, then discards the key before chat without a duplicate authorization or credential cache. Neither path refreshes tokens, starts sign-in, grants Run authority or invokes inference. Revalidate the selected account after authorization and reload current credentials at use.

Interactive startup access has a supervised two-minute human-interaction deadline. Cancellation kills and reaps the helper and restores the terminal according to its contract. Denial, missing/invalid items, identity replacement and timeout report safe startup feedback without a Provider call, sign-in, automatic retry or fallback. Routine reads, writes and deletion retain five-second deadlines. On macOS the OS owns per-item permission persistence; Allow Once may require another prompt on later access.

The keyring helper is a short-lived copy of the same binary. It uses a fixed service, bounded private pipes, a cleared environment and `/` as its working directory. Startup authorization validates the identity inside the helper and returns no credential. Test builds require an absolute `ARANY_TEST_EXE`, and shipped binaries ignore it; synthetic input comes through a private file, not a parent pipe.

## Storage, replacement and recovery

An incompatible or malformed saved account or model preference must not prevent
opening the attached CLI. Show safe recovery guidance and keep access
unconfigured until the user selects valid access. Do not switch billing routes,
load an alternate credential or submit an initial objective automatically.
Recoverable failure during initial `--setup` returns to the composer; terminal
failure and shutdown retain their normal exit behavior.

Setup offers an explicit local reset for invalid selected-route data, with Back
focused by default. Confirmed reset discards only that route's checked account
records, its ChatGPT registration when applicable, and saved model choices.
Conversations, folder trust and other-route accounts remain intact. Snapshot the
bounded records before confirmation and recheck them under the account lock;
concurrent change refuses reset. Cancellation writes nothing. Unsafe or
over-limit filesystem objects cannot be reset through this flow. A partial
storage failure reports uncertainty and does not claim rollback.

Invalid saved model choices can be reset separately, preserving all accounts.
Confirming a reset is a completed local effect even if later setup is cancelled;
subsequent cancellation cannot restore the deleted records.

Reset never interprets an unsupported format, migrates old records, enumerates
the password store or revokes remote access. Explain that old password-store
entries may remain and that reset is local. New setup follows ordinary explicit
storage consent and verified sign-in. Headless commands retain rejection with
actionable attached-setup guidance.

The explicitly approved private-file backend is plaintext at rest beneath a checked, user-owned `0700` root. Each fixed record and pending replacement is bounded, no-follow, single-link, owner-checked and `0600`. On macOS, opened private roots/files/locks reject extended ACL allow entries; ancestors reject ACL mutation grants. Unreadable or unknown ACLs fail closed. Deny-only entries are admissible; more than 128 entries fail closed. Atomic rename with file and directory synchronization prevents a partially written final record; a crash may leave one bounded pending file. Permission modes and ACL checks do not encrypt credentials, exclude same-UID/privileged processes or securely erase old disk contents.

Native Keychain access selects the exact user-domain Keychain, service and slot without changing the default, search list or item permissions. Replacement first reads the selected existing item; only item-not-found permits insertion. Locked or denied access preserves the pinned backend and cannot become a new item or plaintext fallback.

Account readers use only the current OS-user root. They do not inspect or move records from environment-selected Session roots. Native API accounts use schema 2 with explicit nullable effort and Anthropic workspace fields; schema 1 and missing fields reject. ChatGPT registration explicitly carries its nullable issued client ID; index state flags, nullable token/selection and bounded model checks are explicit. Reading unsupported records preserves them; deletion requires the explicit reset described above. Setup and replacement still require current identity, backend and consent admission.

Timed-out OS-store writes report an uncertain outcome distinct from a timed-out read. Keep the backend pinned, and do not claim rollback merely because the helper was killed. ChatGPT logout deletes only its validated token slot, never the native API item. An uncertain deletion keeps account use blocked and warns the user. After a recognized terminal refresh-token error, disconnect the account and clear model evidence before attempting deletion; failed cleanup never re-enables use and remains visible.

The account-replacement lock is held in the same blocking task as the OS credential-store write, so cancelling the awaiting interface cannot release it early.

## Limits and record admission

Native API-account records, their backend marker and private-file records are each at most 1 KiB, in a leaf directory of mode `0700`; a pending file is reused under the lock and published by synced rename. A ChatGPT registration record is 256-byte, carries one stable UUIDv4 host ID and a nullable issued client ID, and never holds a token. `chatgpt-accounts.json` is at most 384 KiB, holds at most eight accounts sharing one host ID and at most 64 check records, each expiring; keyring entries hold metadata only, while private-file entries hold the plaintext token pair. `account-credentials.lock` is a zero-byte file beneath the account root. The separate ChatGPT helper slot admits a versioned, consent-bound token record of at most 64 KiB. Invalid or oversized records reject without partial admission. Pass credentials to or from OS-store helpers only through bounded private pipes, never arguments or process-global environment. Keys and tokens never enter Events, logs, telemetry or drawing calls. Errors are redacted and never expose platform error text or secret records. Availability probes return no existing secret.

A successful read-only store probe does not prove later writes will succeed. Linux targets Secret Service without a KDE/GNOME requirement. OS-store protection does not imply that every backend encrypts at rest or restricts an unlocked item to Arany; [the Linux client-isolation finding](../security/findings/linux-secret-service-client-isolation.md) owns that limitation. The Linux keyring consent warning states that another same-user application may read an unlocked item; it claims no Arany-only secrecy and offers no silent plaintext fallback. Native availability, replacement, unlock/failure behavior, private-file ACL admission and per-item authorization need platform-specific evidence before a support claim.

## Acceptance scenarios and evidence owners

These owners define verification scope, not a claim that every native gate passed. [The account-scope finding](../security/findings/default-account-slot-cross-root.md) and [the stalled-store finding](../security/findings/stalled-credential-store.md) retain security reasoning; [next steps](../../NEXT_STEPS.md#beta-2-and-broader-release) owns outstanding checks. Completed results belong in the implementation handoff and Git/CI history.

| Scenario | Required outcome | Owner |
|---|---|---|
| OS-user scope | Changing Session roots, `HOME` or XDG variables does not change the effective user's account root; different OS users retain separate account state. | [`tests/session_run.rs::account_path_ignores_session_state_environment`](../../tests/session_run.rs), [cross-user finding](../security/findings/default-account-slot-cross-root.md); [isolated Linux keyring lifecycle](../../NEXT_STEPS.md#beta-2-and-broader-release) and [macOS optimized account isolation](../../NEXT_STEPS.md#beta-2-and-broader-release) (native keyring cross-user gates open) |
| Selected identity and billing scope | Valid saved records retain identity and explicit workspace; replacement, schema mismatch and ambient scope substitution reject before disclosure. | [`src/cli/credentials.rs`](../../src/cli/credentials.rs), [`tests/session_run/setup/offline_https/anthropic.rs`](../../tests/session_run/setup/offline_https/anthropic.rs) |
| Backend pinning | Explicit file consent succeeds when admitted; locked/pinned keyring failure never selects file or environment credentials. | [`src/cli/credentials.rs`](../../src/cli/credentials.rs), [`tests/session_run/setup.rs`](../../tests/session_run/setup.rs) |
| Native macOS storage | Save/read and replacement reach the isolated native Keychain; stale selected identity and locked/denied access reject, token deletion uses the supervisor, user Keychain preferences remain unchanged, and unsafe private-file ACLs reject. | [`src/cli/credentials.rs::native_store_round_trips_an_isolated_synthetic_account`](../../src/cli/credentials.rs), [`src/private_fs/acl.rs`](../../src/private_fs/acl.rs), [`tests/session_run.rs`](../../tests/session_run.rs); the isolated debug Keychain gate runs in `macOS tests (debug)`; human Keychain dialogs: [item 8: Native macOS stores and clipboard](../../NEXT_STEPS.md#beta-1-final-user-owned-checks) (open) |
| Selected startup item | Folder consent precedes selected-item authorization, which precedes catalog/chat; unrelated sources launch no credential helper and account changes reject. | [`src/cli/attached/run.rs`](../../src/cli/attached/run.rs), [`src/cli/credentials/keyring_helper.rs`](../../src/cli/credentials/keyring_helper.rs); [isolated Linux keyring lifecycle](../../NEXT_STEPS.md#beta-2-and-broader-release), [macOS optimized account isolation](../../NEXT_STEPS.md#beta-2-and-broader-release) and [item 8: Native macOS stores and clipboard](../../NEXT_STEPS.md#beta-1-final-user-owned-checks) (open) |
| Slow authorization and cancellation | Human interaction can exceed five seconds within the two-minute bound; interruption restores the terminal, kills/reaps the helper and submits no task. | [`src/cli/credentials/keyring_helper.rs::authorization_wait_accepts_human_latency`](../../src/cli/credentials/keyring_helper.rs), [`src/cli/attached/run.rs`](../../src/cli/attached/run.rs); the ignored six-second gate runs in `Linux Tests (debug)`, `Linux Tests (release)`; native OS dialog: [item 8: Native macOS stores and clipboard](../../NEXT_STEPS.md#beta-1-final-user-owned-checks) (open) |
| Failed initial or resumed access | No automatic objective submission, Provider Run, alternate route or replay mutation follows failed startup authorization. | [`src/cli/attached/run.rs`](../../src/cli/attached/run.rs), [`tests/session_run/setup.rs`](../../tests/session_run/setup.rs); [isolated Linux keyring lifecycle](../../NEXT_STEPS.md#beta-2-and-broader-release), [macOS optimized account isolation](../../NEXT_STEPS.md#beta-2-and-broader-release) and [item 8: Native macOS stores and clipboard](../../NEXT_STEPS.md#beta-1-final-user-owned-checks) (open) |
| Current account format and root | Only current records in the OS-user root authorize access; unsupported or malformed records reject without conversion or automatic deletion. Attached startup remains available for explicit recovery. | [`src/cli/credentials.rs`](../../src/cli/credentials.rs), [`src/cli/chatgpt/account/tests.rs`](../../src/cli/chatgpt/account/tests.rs), [`src/cli/attached/run.rs`](../../src/cli/attached/run.rs) |
| Local setup recovery | Invalid selected-route data keeps the composer and setup available; Back preserves records, confirmed reset permits fresh setup, and concurrent replacement or unsafe files prevent deletion. Other accounts and closed conversation history remain intact. | [`src/cli/attached/run.rs`](../../src/cli/attached/run.rs), [`src/cli/credentials.rs`](../../src/cli/credentials.rs), [`src/store/state.rs`](../../src/store/state.rs) |
| Routine timeout and uncertain effects | Stalled helpers are killed/reaped within routine bounds; uncertain writes/deletion cannot claim rollback or restore blocked access. | [`src/cli/credentials/keyring_helper.rs::timed_out_write_is_uncertain_and_reaps_helper`](../../src/cli/credentials/keyring_helper.rs), [stalled-store native gate](../security/findings/stalled-credential-store.md); the ignored write-timeout gate runs in `Linux Tests (debug)`, `Linux Tests (release)`; [isolated Linux keyring lifecycle](../../NEXT_STEPS.md#beta-2-and-broader-release) (open) |
| Real native store lifecycle | Isolated synthetic items must prove save/read/replacement, item authorization and failure/deletion without accessing the user's live account store. | [`src/cli/credentials.rs::native_store_round_trips_an_isolated_synthetic_account`](../../src/cli/credentials.rs); `macOS tests (debug)` (debug Keychain fixture); [isolated Linux keyring lifecycle](../../NEXT_STEPS.md#beta-2-and-broader-release), [macOS optimized account isolation](../../NEXT_STEPS.md#beta-2-and-broader-release) and [item 8: Native macOS stores and clipboard](../../NEXT_STEPS.md#beta-1-final-user-owned-checks) (open) |
