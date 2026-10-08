# Account fixture reached live legacy metadata

- Severity: P1
- Status: Implementation corrected; affected local authentication requires fresh sign-in or an external metadata backup
- Observed: 2026-10-08, native macOS debug suite

## Failure and impact

The catalog subprocess owner set a debug-only `ARANY_TEST_ACCOUNT_ROOT` after clearing its environment, but left the legacy default root unisolated. HOME/XDG fallback selected the real passwd home. The newly completed ChatGPT migration copied live registration/index metadata to the fixture and replaced the source with version-zero markers. The command then failed on the credential-helper deadline, and panic cleanup removed the temporary destination. No successful live inference was recorded. Migration performs no Keychain deletion; the index and registration cannot be reconstructed from their markers or SQLite provenance.

The failure requires a real legacy account plus a disposable override destination. Private record admission and migration locks do not prevent it: both roots can legitimately pass their controls. Test isolation must cover both roots before opening either.

## Correction and verification

Every affected catalog launch and adjacent saved-account/native-PTY fixture now sets synthetic absolute HOME/XDG roots. Debug `StateRoot::default_path` rejects a legacy path outside the override's non-root parent before migration can open it. Release account-root overrides still reject; ordinary Session/account behavior is unchanged. [Testing rules](../../../agents/testing.md) own fixture setup.

The existing [account-path subprocess owner](../../../tests/session_run.rs) checks confined stable/legacy roots and ambient-root rejection; the affected catalog and cost-consent owners pass. Full native offline debug/release suites pass after correction. [Dated evidence](../../research/testing-strategy-for-rust-cli-harness.md#native-macos-baseline-2026-10-08) records the incident, scoped checks and local recovery disposition. Native Linux re-verification, macOS ACL and isolated Keychain lifecycle remain separate gates.

Local recovery moved only the two validated 33-byte migration markers into a private backup under the account lock, preserving their bytes and leaving Keychain and Session history unchanged. Fresh setup is available again, but reauthorization remains user-owned and the original metadata was not recovered.
