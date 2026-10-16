# Finding: different StateRoots share one API-account keyring slot

Status: **partially repaired; isolated Linux keyring setup and cross-user proof, and optimized macOS account gates remain open**. Severity: **medium (local account-scope confusion and replacement race if backend coordination fails)**. The user chose one account scope per OS user across that user's Sessions and StateRoots, not one account per StateRoot or the whole machine. No real account or fixed keyring slot was touched by the repair.

## Evidence and boundary

The Session [default StateRoot](../../../src/store/state.rs) follows `directories::ProjectDirs`, but Linux `account_path` derives the effective UID's passwd home and a fixed `.local/state/arany` suffix. macOS derives the same OS identity with its `Library/Application Support/dev.Arany.arany` suffix. [Credential save and inspection](../../../src/cli/credentials.rs) and [ChatGPT registration](../../../src/cli/chatgpt/registration.rs) use that path for their marker and [replacement lock](../../../src/store/state/lock.rs). The account-path owner runs through bounded subprocess capture in [the product test target](../../../tests/session_run.rs): altered `HOME` and XDG variables cannot change account scope; relative fixtures and optimized overrides reject. The fixed keyring slot remains `default-native-api-account`, selected only after current account-root admission.

Current readers admit only current records in the OS-user root. Unsupported formats reject unchanged; attached setup offers explicit local reset without interpreting or relocating them. The [credential contract](../../specs/credentials.md#storage-replacement-and-recovery) owns confirmation, concurrent-change rejection and partial-failure behavior. Other same-UID access to an unlocked Secret Service item is a separate [finding](./linux-secret-service-client-isolation.md).

Tests: [`src/cli/credentials.rs::native_store_round_trips_an_isolated_synthetic_account`](../../../src/cli/credentials.rs); [`tests/session_run/setup.rs::release_saved_file_account_is_shared_across_session_roots`](../../../tests/session_run/setup.rs); [`tests/session_run/setup.rs::release_selected_chatgpt_account_starts_without_keyring_or_run`](../../../tests/session_run/setup.rs).

Non-default-slot keyring fixtures do not prove shipped setup, cross-user isolation, native write atomicity or rollback after timeout. Private-file and token-free ChatGPT startup fixtures do not prove live sign-in, token loading or paid inference. Debug account-root overrides are fixture-only; optimized builds reject them.

## Resolution and verification

Tests: [`tests/session_run/setup.rs::release_saved_account_is_private_across_os_users`](../../../tests/session_run/setup.rs); [`tests/session_run/setup/offline_https.rs::release_private_file_setup_reuses_saved_account`](../../../tests/session_run/setup/offline_https.rs).

These private-file product fixtures use a private passwd home and namespace; compiled HTTPS catalog and direct-turn replies use synthetic data under ordinary TLS validation. They do not prove isolated keyring behavior, same-UID secrecy or live Provider compatibility.

Keep the OS-user account marker and replacement lock independent of environment-selected Session StateRoots, matching the OS user's keyring slot. Refuse unsupported records unchanged and require explicit setup confirmation for reset. The private file mode does not isolate same-UID processes, and the effective user's passwd home must pass StateRoot path/owner checks. [The account-path owner](../../../tests/session_run.rs), [ACL corpus](../../../src/private_fs/acl.rs) and [isolated Keychain owner](../../../src/cli/credentials/macos_tests.rs) define their separate verification scopes. Optimized native product setup and cross-user account/keyring behavior remain unverified.

Open gates: **isolated Linux keyring lifecycle** and **macOS optimized account isolation** in [next steps](../../../NEXT_STEPS.md#beta-2-and-broader-release). Never overwrite the live fixed slot; private Session State alone does not isolate an OS credential service.

An isolated Secret Service alone cannot complete first-run setup offline: the shipped wizard reads the selected Provider's compiled HTTPS catalog before saving. The network-namespace fixture supplies that catalog and a synthetic direct-turn response through namespace DNS and trust inputs, without a product endpoint override or weaker endpoint admission.
