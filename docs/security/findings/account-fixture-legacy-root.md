# Account fixture reached live legacy metadata

- Severity: P1
- Status: Implementation corrected; affected local authentication requires fresh sign-in or an external metadata backup
- Observed: 2026-10-08, native macOS debug suite

## Failure and impact

The catalog subprocess owner set a debug-only `ARANY_TEST_ACCOUNT_ROOT` after clearing its environment, but left the legacy default root unisolated. HOME/XDG fallback selected the real passwd home. The newly completed ChatGPT migration copied live registration/index metadata to the fixture and replaced the source with version-zero markers. The command then failed on the credential-helper deadline, and panic cleanup removed the temporary destination. No successful live inference was recorded. Migration performs no Keychain deletion; the index and registration cannot be reconstructed from their markers or SQLite provenance.

The failure requires a real legacy account plus a disposable override destination. Private record admission and migration locks do not prevent it: both roots can legitimately pass their controls. Test isolation must cover both roots before opening either.

## Correction and verification

On 2026-10-09, pre-beta account relocation was removed entirely. Current account entry points read only the OS-user root and never copy or tombstone old-root records. Synthetic HOME/XDG isolation and the debug ambient-root refusal remain required for environment-selected Session/preference state. This removes the migration path responsible for the incident; it does not restore the lost metadata.

Every affected catalog launch and adjacent saved-account/native-PTY fixture now sets synthetic absolute HOME/XDG roots. Debug `StateRoot::default_path` rejects an environment-selected Session/preference path outside the override's non-root parent before ambient state access. Release account-root overrides still reject. [Testing rules](../../../agents/testing.md) own fixture setup.

The [account-path subprocess owner](../../../tests/session_run.rs) checks confined account and Session/preference roots and ambient-root rejection. [Dated evidence](../../research/testing-strategy-for-rust-cli-harness.md#native-macos-baseline-2026-10-08) records the incident, the affected catalog/cost-consent checks, the corrected suites and local recovery disposition. Native Linux re-verification and optimized macOS product account setup/cross-user evidence remain separate gates; focused ACL and isolated Keychain evidence does not substitute for them.

Local recovery moved only the two validated 33-byte migration markers into a private backup under the account lock, preserving their bytes and leaving Keychain and Session history unchanged. Fresh setup is available again, but reauthorization remains user-owned and the original metadata was not recovered.
