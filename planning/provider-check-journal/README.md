# Journal standalone Provider checks

## Metadata

- Status: `In progress`
- Owner: repository owner; prerequisite requested in the 2026-10-22 plan review
- Last updated: `2026-10-22`
- Research gate: passed; scoped peer findings and rationale in [D-23](../../docs/research/next-step-decision-register.md#d-23--standalone-provider-check-durability).
- Plan gate: passed; owner decisions recorded below, no owner-only question remains.
- Next action: C2. C1 custom/native implementation is complete; both slices precede S1 of [Provider setup UI](../provider-setup-ui/README.md).

## Outcome and scope

Every standalone `arany provider check` records intent, each diagnostic request and its outcome before continuing, without fabricating a conversation or Run. The typed checker also serves the future UI. The remaining slice covers selected ChatGPT checks, with no new probes, credential authority, retries or setup editors.

Implemented custom/native behavior, limits and recovery now belong to [Provider commands](../../docs/specs/provider.md#commands) and [Execution's standalone check contract](../../docs/specs/execution.md#standalone-provider-checks). The existing CheckJournal, CheckReducer, CheckView and Store thread own the reusable lifecycle. SQLite schema 6 includes the reserved ChatGPT target variant; no further SQLite or Session Event bump is needed for C2.

## Research and remaining decisions

[CLI dispatch](../../crates/arany-cli/src/cli/provider.rs) routes ChatGPT checks to the [account owner](../../crates/arany-cli/src/accounts/chatgpt.rs). It clears account-bound evidence, loads the catalog, calls `probe_chatgpt_model` and records optional model evidence in the account index. These catalog/probe calls remain unjournaled until C2 routes them through the shared journal. The account root is OS-user-owned and its CLI rejects `--state-dir`; preserve both constraints. Public conformance entry points must route through the journal owner; protocol helpers require its committed call token before egress.

Decided 2026-10-22 (**lead defaults required by guarantees**): use `StateRoot::account_path()` and that root's `events.sqlite3`. If it also holds chat history, both streams share the Store's capacity and schema. Do not create fake Sessions, Goals or Runs, a second database, a generic stream framework or a new dependency. No Workspace content enters a check. Provider adapters retain encoders and sanitized error mapping; the account/CLI edge supplies selected source and consent, never raw SQL or terminal state.

### Account evidence and recovery

Decided 2026-10-22 (**guarantee default**): commit start, clear evidence through the checked account writer, then acknowledge clearing before catalog access. A new attempt suppresses earlier evidence for that target even if physical clearing fails. Selected account credentials and existing consent gates remain authoritative; a check cannot bootstrap another account or billing source.

Evidence stays in the account index and gains the non-secret CheckId. After every fixed probe passes, revalidate the exact account snapshot and commit the publication intent. The existing locked account writer revalidates identity and publishes the model-check record, then the journal commits terminal success. This is not a cross-store transaction: JSON evidence is unusable until the final acknowledgment. Readers require the latest target attempt to be replay-valid and successful with matching fingerprint/contract and normal freshness checks. Retained account-index CheckIds must also protect their proof from Start-time pruning. No token or consent receipt moves into SQLite. Account admission remains independent of optional checks; failure removes only diagnostic/window evidence.

Reuse the root check lease across the bounded operation; never hold the account-replacement lock across network requests. Concurrent checks refuse busy. Account editors may proceed, but publication refuses a changed snapshot. A crash between JSON publication and journal success remains unknown. Shared recovery never repairs JSON, completes publication or retries a call. Explicit fresh checks receive a new CheckId.

CLI output reports CheckId and durable disposition, or unconfirmed status after a lost final acknowledgment, matching custom/native output. Update exact-output fixtures and return the shared typed view to future UI callers.

### Stored format

Decided 2026-10-22 (**lead default; [migration rule](../../AGENTS.md#global-rules)**): advance account-index JSON schema **1 → 2** to require CheckId on new model checks. Register its migration and canonical fixture. Clear old optional model checks without changing identities, tokens, selections or consent fields. Advance only the optional model-check fingerprint, not account-admission authority. Provider-route JSON formats stay unchanged. No runtime compatibility reader is added.

### Security and bounds

The [security pass](../../agents/security.md#security-pass-triggers) covers paid inference, credential lookup, private journal state and error rendering. Authority is explicit user diagnostic consent through existing `--accept-cost` or UI confirmation; no new consent flag or raw shell/curl path. Models and Workspace input cannot initiate checks, choose destinations or extend limits. Existing transport and credential isolation remain authoritative.

Reuse [Execution's lifecycle/storage limits](../../docs/specs/execution.md#standalone-provider-checks) and the [ChatGPT request contract](../../docs/specs/chatgpt.md#catalog-checks-and-runs): at most three inference calls, existing catalog allowance, no retries, existing aggregate probe deadline and local-only output caps. Narrow stages by the lifecycle deadline and reserve local settlement time. Admission reuses the shared retention rule and refuses unavailable capacity before calls. No prompts, response text, environment values, raw errors or credentials enter Events, output or telemetry.

Linux/macOS reuse SQLite and existing private filesystem adapters; no new platform mechanism. Native locking, ACL and interruption claims still require their existing evidence owners.

## Remaining vertical slice

**C2 — Journal selected ChatGPT CLI checks through the same interface.** Account selection/consent → account-root CheckJournal → catalog/probes → checked JSON publication → terminal Event/output. Ship account-index schema 2 migration/fixtures and [ChatGPT contract](../../docs/specs/chatgpt.md#catalog-checks-and-runs) updates together. Execution owns stream/recovery changes, if any. **Usable alone:** all shipped check variants share the durable lifecycle, ready for setup S1.

| Scenario | Required observation / existing evidence owner |
|---|---|
| Selected ChatGPT CLI | Correct account root and consent; acknowledged intent/result around each catalog/probe request; three probes and optional window metadata preserved; no Session/Run. Extend [account corpus](../../crates/arany-cli/src/accounts/chatgpt/account/tests.rs) and [checked-turn journey](../../crates/arany-cli/tests/session_run/setup/offline_https/chatgpt/checked_turn.rs). |
| Failed admission and ordering | No HTTP after refused append, unavailable selected credential, failed probe or exhausted allowance. Changed account snapshot refuses publication. Extend account/checker cases and the shared CheckReducer transition corpus. |
| Crash and uncertainty | Kill before dispatch, after delivery, before result acknowledgment and between JSON publication/journal finish. Reopen without HTTP; incomplete evidence is unusable while ordinary account access remains valid. Concurrent entry cannot recover a live check. Extend [Store crash corpus](../../crates/arany/src/store/journal/crash_tests.rs) and checked-turn process owner. |
| Migration and stale contract | Exact schema-1 fixture reaches 2, preserving credential/identity/consent fields and clearing optional checks. Interrupted migration and malformed records fail safely. Extend account migration fixtures. |
| Privacy and capacity | Selected-source/Workspace canaries stay absent from output, Events, database and telemetry. Missing credentials and full-store/check count refuse before calls. Extend existing account/privacy and Store admission owners. |

Research → Plan → Implement gates apply; a contradictory finding stops the slice for a plan update. Follow current repository verification instructions. Synthetic evidence is not native/live support; remaining work belongs in [NEXT_STEPS](../../NEXT_STEPS.md#beta-2-and-broader-release).

## Exit criteria

- Every CLI check and callable UI interface enforce acknowledged intent, validated outcome and effect-free replay.
- Account migration and crash scenarios prove evidence linkage without changing ordinary credential admission or adding legacy runtime readers.
- Specs own implemented behavior, limits and recovery; D-23 owns rationale.
- After C2, unblock setup S1, remove this plan/index row and update its dependent link under the [planning lifecycle](../README.md#lifecycle).
