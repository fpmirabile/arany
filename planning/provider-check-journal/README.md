# Journal standalone Provider checks

## Metadata

- Status: `Ready`
- Owner: repository owner; prerequisite requested in the 2026-10-22 plan review
- Last updated: `2026-10-22`
- Research gate: passed; code, contracts and scoped primary peer evidence below.
- Plan gate: passed; no owner-only question remains. Implementation has not started.
- Next action: C1, then C2. Both precede S1 of [Provider setup UI](../provider-setup-ui/README.md).

## Outcome and scope

Every standalone `arany provider check` records its intent, each diagnostic request and its outcome durably before continuing, without fabricating a conversation or Run. The same typed checker serves the future UI. This feature closes the journal-first gap for custom, native OpenAI/Anthropic and ChatGPT checks; it does not change the probes, confer credential authority, introduce retries, or implement setup editors.

## Research

| Owner | Verified finding |
|---|---|
| [Custom checker](../../crates/arany/src/provider/custom/check.rs) | `check_custom_profile` opens Store, clears evidence, admits the destination, then reads the selected environment key and directly dispatches probes. It writes/re-reads evidence but appends no Event. Request-local Session/Run IDs do not create persisted Sessions/Runs. |
| [Native checker](../../crates/arany/src/provider/native_check.rs) | Both environment-backed and saved-credential paths reach `check_native_model_with_credentials`: clear evidence, catalog lookup, direct probes, evidence publication. No journal call surrounds the requests. |
| [CLI dispatch](../../crates/arany-cli/src/cli/provider.rs), [ChatGPT checks](../../crates/arany-cli/src/accounts/chatgpt.rs) | CLI dispatch includes custom, native and ChatGPT checks. ChatGPT clears account-bound evidence, loads the catalog, calls `probe_chatgpt_model` and records model evidence in the account index; it also lacks per-probe Events. Its account root is OS-user-owned, and its CLI rejects `--state-dir`. |
| [Journal](../../crates/arany/src/journal.rs), [Session reducer](../../crates/arany/src/session/reducer.rs) | `Journal::append` requires a SessionId and reduces Session Events. A standalone check cannot use that interface merely by generating an ID. |
| [Store schema](../../crates/arany/src/store/journal.rs), [evidence](../../crates/arany/src/store/evidence.rs) | `events.sqlite3` schema 5 has Session `events`, custom/native evidence and migration tables. Evidence caches are not an append-only diagnostic journal. |
| [Migration registry](../../crates/arany/src/store/migrate.rs), [execution contract](../../docs/specs/execution.md#journal-migration) | Registered staged SQLite migrations already exist (including 4 → 5), with exact-schema checks, backup and recovery. [Provider commands](../../docs/specs/provider.md#commands) still say a schema change “would force a history reset”; that statement conflicts with the current migration contract and must be corrected in C1. |

### Peer qualification and recording

Primary sources re-fetched on 2026-10-22, in Claude/Codex-first order:

- [Claude gateway verification](https://code.claude.com/docs/en/llm-gateway-connect#verify-the-connection): “With the variables exported in your shell, send a one-token request to the gateway directly.” Its [rollout test](https://code.claude.com/docs/en/llm-gateway-rollout#test-claude-code-against-the-gateway) then uses the actual client and checks gateway logs.
- [Codex gateway requirements](https://learn.chatgpt.com/docs/enterprise/roll-out-a-gateway#gateway-requirements): “A health endpoint, `/v1/models`, Chat Completions response, or one plain-text reply does not qualify the gateway.” [Qualification](https://learn.chatgpt.com/docs/enterprise/gateway-compatibility#qualification-checklist) requires evidence for the deployed combination, including streaming, Tools, follow-up turns and failure behavior.
- [OpenCode authentication](https://opencode.ai/docs/cli/#auth), [models](https://opencode.ai/docs/cli/#models) and [custom Providers](https://opencode.ai/docs/providers/#custom-provider) describe credentials, declarations, catalog refresh and selection, without defining an exact-conformance journal in those sections.
- [Pi, pinned a13d35a, integration tests](https://github.com/earendil-works/pi/blob/a13d35a742c6ef8462812a28fbe1d8c8b7431c32/packages/coding-agent/docs/custom-provider.md#test-the-integration): “Adapt the relevant suites rather than relying only on manual prompts.” It covers protocol failures, usage, cancellation and authentication; observation hooks are not specified as durable records.

These inspected documents support testing the real configured protocol and meaningful failure paths. They do **not specify** a durable standalone-probe intent/outcome stream, reducer or crash/replay contract. This is a scoped documentation finding, not a claim that no peer implementation records calls. Arany's exact probe suite stays as contracted; its durable recording diverges from the documented direct-probe/gateway-log workflows to satisfy [journal-first durability, honest uncertainty and strict replay](../../AGENTS.md#non-negotiable-guarantees).

## Chosen direction

Decided 2026-10-22 (**lead default required by guarantees**): add a CheckId-keyed journal in the existing Store database. Reuse the Store thread, checked database opening, transactions, admission bounds and migration machinery. Do not create fake Sessions/Goals/Runs, a second database, a generic stream framework or a new crate/dependency.

### Stream, ownership and reducer

- Add `provider_check_events` to `events.sqlite3`: `sequence INTEGER PRIMARY KEY`, `check_id TEXT NOT NULL`, `target_key BLOB NOT NULL` (32-byte non-secret identity digest), `kind TEXT NOT NULL`, `event_version INTEGER NOT NULL`, bounded JSON `payload`, and `created_at_ms INTEGER NOT NULL`; use a STRICT table and indexes on `(check_id, sequence)` and `(target_key, sequence)`. The target key identifies the exact custom name or native/account tuple; it never hashes a raw secret without the existing keyed fingerprint construction. Sequence belongs to this table. No Session/Run foreign key or placeholder is present.
- Custom/native checks use the selected private StateRoot. ChatGPT uses `StateRoot::account_path()` and that root's `events.sqlite3`, preserving its existing OS-user scope and rejection of `--state-dir`. If that root also hosts chat history, both streams share one Store schema and capacity. Neither check path consumes Workspace content.
- Add a narrow `CheckJournal` facade in the library journal owner, backed by the existing Store thread, with typed start/call/result/publication/finish operations and a read-only `CheckView`. Its append acknowledgment follows transaction commit. UI progress consumes acknowledged views; the Session event tap remains Session-only. Provider-specific encoders and sanitized error mapping stay with their adapters; the CLI/account edge supplies selected source and consent, never raw SQL or terminal state.
- Add a pure `CheckReducer`, separate from `SessionView`, at the check journal owner. Store replays the selected check, applies the candidate Event and commits only a valid transition in one immediate transaction. Whole-store validation, migration validation and read-only check inspection also validate these streams. A malformed check stream cannot be hidden by valid Session history.
- All kinds start at payload version 1 with a closed check-format registry. This is a distinct typed diagnostic Event family, not unvalidated JSON or reusing `ProviderCallRecorded` without its required Run lifecycle.

| Event | Reducer rule |
|---|---|
| `ProviderCheckStarted` | Exactly once per CheckId. Pins check-contract version, source/identity fingerprint, selected route/model/efforts, explicit cost consent and the bounded ordered catalog/probe manifest and deadlines. No credential bytes, credential hashes usable for guessing, prompts or response bodies. |
| `ProviderCheckEvidenceCleared` | Matches the start target and confirms invalidation of previous evidence before catalog/probe calls. It cannot occur twice or after a call. |
| `ProviderCheckCallStarted` | Only after evidence clearing, and only the next manifest operation, with its ordinal, kind, destination identity and remaining call/output/time allowance; at most one pending request. Commit acknowledgment is mandatory before transport dispatch. Catalog requests have a distinct kind and do not consume an inference slot. |
| `ProviderCheckCallFinished` | Matches the pending ordinal exactly; records strict acceptance or a closed rejection/failure/cancellation/unknown disposition and known usage or explicit unknown usage. Any non-success prevents further probes. Duplicate, skipped or reordered results reject. |
| `ProviderCheckEvidenceStarted` | All required probes passed and no call is pending; pins the exact target/evidence digest and declaration/account snapshot after revalidation. Commit before publication. |
| `ProviderCheckFinished` | One terminal result. Success requires every required probe and confirmed evidence publication; failure/cancelled/Uncertain cannot authorize evidence. No subsequent dispatch or append other than read-only inspection. |

Custom admission remains declaration → destination/DNS admission → selected environment-key lookup. Native/ChatGPT credentials retain their existing selected-source and consent gates; no check can bootstrap a different account or billing source. The manifest admits the existing catalog stage with at most 16 sequential pages, ending on an observed catalog completion; unused page allowance is not a required probe. It then requires every fixed inference probe in order. Request-local synthetic IDs remain transport data and are never interpreted as persisted chat identities.

### Evidence, cancellation and recovery

Decided 2026-10-22 (**guarantee default**): link newly published evidence to its CheckId. Starting a new attempt suppresses previous evidence for that target even if physical clearing fails. Custom/native start, cache invalidation and `ProviderCheckEvidenceCleared` commit in one Store transaction before egress; ChatGPT commits start, clears through its checked account writer, then acknowledges clearing before catalog access. Custom/native evidence insertion and successful `ProviderCheckFinished` commit atomically in the same SQLite transaction, after the separately acknowledged publication intent. Evidence readers require the latest target attempt to be a replay-valid successful stream with matching fingerprint/contract and ordinary freshness/DNS checks; the cache row alone is insufficient.

ChatGPT evidence stays in its account index and gains the non-secret CheckId. Its existing locked account writer revalidates identity, publishes the check record, then commits the terminal check Event in the account-root journal. This is not a cross-store transaction: until that final acknowledgment, the JSON evidence is unusable. A crash between stores yields unknown publication, never inferred success. Account admission itself remains independent of optional checks; failure only removes diagnostic/window evidence. No token or consent receipt moves into SQLite.

Use one checked, zero-byte `provider-check.lock` per journal root, with the existing no-follow/owner/ACL lock machinery, held by the check owner across its bounded operation; do not hold the account-replacement lock across network requests. Another check refuses busy rather than queues. Route editors may proceed; evidence publication rechecks the exact declaration/account snapshot and refuses stale results. This lease also prevents recovery from marking another live process's check interrupted.

Cancellation stops further dispatch and records the observed outcome; after a dispatched request without a confirmed response, usage/outcome is Uncertain. If append acknowledgment is lost, do not dispatch or claim success. Read-only replay projects unfinished checks as interrupted/Uncertain without performing I/O. On the next writable check entry, after acquiring the lease, append an unknown call result when a call is pending, then the terminal Uncertain disposition for abandoned streams before admitting a new CheckId. Never resend a probe, repair JSON or complete evidence publication during recovery. An explicit fresh check is a new operation, not continuation of the uncertain one.

CLI success/failure output reports the CheckId and durable disposition; a missing final acknowledgment reports unconfirmed status. C1/C2 update exact-output fixtures. The future UI uses the same typed interface and `CheckView`; it never calls an unjournaled probe helper. Public conformance entry points must route through this owner; protocol-level synthetic helpers require its committed call token before egress.

### Stored formats and SQLite

Decided 2026-10-22 (**lead default; [migration rule](../../AGENTS.md#global-rules)**): C1 advances SQLite **5 → 6** with a registered exact-schema step and fixture. Add the check table/index and `check_id` linkage to the custom/native evidence tables. Clear those derived caches during migration because their old rows prove no check journal; do not manufacture historical probe Events. Preserve Session Events, their bytes/digests and account credentials. Existing 4 → 5 migration composes with the new step; unsupported/below-floor history follows the existing intact archive-and-notice rule, not silent reset. Bump custom/native check fingerprints so unjournaled evidence cannot remain current.

Session Event payload versions and compaction compiler stay unchanged in C1. Register new diagnostic kinds with floor/current 1 and canonical serialization; no fictional v0 migration is needed, but current identity/replay fixtures are mandatory. Extend exact-schema inspection, staged migration/recovery and read-only validation to the new table. No runtime compatibility reader is added. Replace Provider's stale schema-reset claim with a link to the current migration contract in the implementation PR.

C2 advances the ChatGPT account-index JSON schema **1 → 2** to require CheckId on new model-check entries; register its migration and fixture. The step clears old optional model checks, leaving account identities, tokens, selections and consent fields unchanged. Advance only the optional model-check fingerprint contract, not account-admission authority. Reuse C1's declared ChatGPT stream variant and table; no further SQLite or Session Event bump is required. The Provider-route JSON formats are untouched.

Linux/macOS use the same SQLite/record logic and existing private filesystem adapters. This plan adds no platform mechanism. Native interruption, locking and ACL support claims still require the existing native evidence owners.

## Security and resource model

The [security pass](../../agents/security.md#security-pass-triggers) covers paid inference, credential lookup, private journal state and error rendering. The authority is explicit user diagnostic consent: the existing custom-check invocation, `--accept-cost` for native/ChatGPT, or the UI confirmation. No new CLI consent flag is introduced. Consent is bounded to a fixed synthetic probe manifest and admitted destination; peer examples never authorize a raw shell/curl path inside Arany. Existing Provider transport and credential isolation remain authoritative. Models/Workspace input cannot create checks, select destinations, extend budgets or read diagnostic records through a new Tool.

Reuse the [Provider](../../docs/specs/provider.md#commands) and [ChatGPT](../../docs/specs/chatgpt.md#catalog-checks-and-runs) request limits: custom ≤15 inference calls, native/ChatGPT ≤3, no retries, the existing 180-second probe deadline and existing remote versus local-only output caps. Wrap the entire new check lifecycle in a 240-second ceiling, narrowing each existing stage deadline; reserve five seconds for final local settlement and never hold a request past its earlier limit. Expiry still permits unknown, never an automatic retry or a claim of remote cancellation.

New local bounds (**lead defaults needed for durable storage**): at most 48 Events/check (including up to 16 catalog pages, three native probes and recovery/publication facts), 4 KiB per payload, 1,024 retained check streams/root and one active check/root; the existing 256 MiB database ceiling and storage headroom remain shared with Sessions. The fixed manifest also bounds catalog requests by their existing catalog contract. Reserve the entire worst-case local check footprint before its first call. Reaching any bound rejects new work without truncation, automatic deletion, a queue or quota widening. Store request channels and acknowledgments keep their existing bounds. No new logging of prompts, raw errors, tokens, environment values or reply text is allowed.

## Contract and scenarios

[Execution](../../docs/specs/execution.md) owns the separate stream, transitions, migration and recovery; [Provider](../../docs/specs/provider.md#commands) owns custom/native diagnostic behavior; [ChatGPT](../../docs/specs/chatgpt.md#catalog-checks-and-runs) owns its account-bound optional evidence. Update these owners with their slice. Evidence extends existing corpora, not new end-to-end frameworks:

| Scenario | Required observation / evidence owner |
|---|---|
| Standalone custom and native CLI | No Session or Run is created; each request is preceded by an acknowledged intent and followed by its result before the next dispatch. Closed/reopened CheckView matches output and exact evidence. Extend [custom CLI journey](../../crates/arany-cli/tests/session_run/custom.rs), [native checker corpus](../../crates/arany/src/provider/native_check.rs) and [setup HTTPS journey](../../crates/arany-cli/tests/session_run/setup/offline_https.rs). |
| Ordering and admission failures | No HTTP after refused append, invalid destination, absent selected key, failed probe or exhausted budget. Reject forged order, duplicate ordinals, premature success and changed snapshots. Extend checker tests and a focused CheckReducer transition table at the new owner; existing [journal tests](../../crates/arany/src/store/journal/tests.rs) own transaction failure fixtures. |
| Crash and uncertainty | Kill before dispatch, after request delivery, before result acknowledgment and between evidence intent/publication/finish. Reopen without network; no evidence from an incomplete stream, no repeated probe. A concurrent process cannot recover a live check. Extend [crash corpus](../../crates/arany/src/store/journal/crash_tests.rs) and existing process owners. |
| ChatGPT CLI | Selected account/root, cost consent, three probes and optional window metadata remain correct. Crash after JSON publication but before journal finish makes that evidence unusable while valid account access remains available. Extend [ChatGPT account corpus](../../crates/arany-cli/src/accounts/chatgpt/account/tests.rs) and [checked-turn journey](../../crates/arany-cli/tests/session_run/setup/offline_https/chatgpt/checked_turn.rs). |
| Migration and stale contract | Exact schema-5 fixture with Session history and old evidence reaches 6 with identical Session replay and cleared derived caches; floor-4 chain works; crash/staging recovery and malformed/extra schema objects fail safely. Account-index migration preserves credential fields exactly and drops optional checks. Extend [migration fixtures](../../crates/arany/src/store/migrate/tests.rs) and account fixtures. |
| Secret/Workspace canaries and capacity | Invalid custom destination performs zero environment lookups; admitted destination reads only the selected variable. Missing key produces no probe. No canary enters output, Events, database, errors or telemetry. Full store/check count refuses before dispatch. Extend existing custom/native/account privacy and Store admission cases. |

## Risks and mitigations

- A parallel Event family evades validation → every write/reopen/migration uses the CheckReducer; Session APIs remain incapable of accepting check Events.
- Evidence outlives its proof → explicit CheckId linkage and successful replay required; old derived evidence is invalidated, never upgraded into fabricated proof.
- Shared database migration is treated as a reset → additive registered schema step, exact fixtures and corrected Provider wording; intact archive policy only where migration is unsupported.
- Check work becomes a general task runner → fixed manifest, bounded callbacks and synthetic input only; no Goals, Tools or arbitrary payload API.

## Vertical slices

1. **C1 — Journal standalone custom and native CLI checks end to end.** CLI `provider check custom:NAME|openai|anthropic` → existing checker/admitted adapter → CheckJournal/Store/CheckReducer → linked evidence → durable CLI result. Ship schema 6 and its migration/fixtures, bounded locks/recovery, the shared typed interface and changed Provider/Execution contracts together. Exercise successful, refused, interrupted and replayed checks through the existing process/Store owners. **Usable alone:** the existing custom/native CLI checks have durable history and honest interrupted outcomes without the setup UI.
2. **C2 — Journal selected ChatGPT CLI checks through the same interface.** Account selection/consent → account-root CheckJournal → catalog/probes → checked JSON publication → terminal Event/output. Ship account-index schema 2 migration/fixtures and ChatGPT contract updates together; verify uncertainty across JSON/SQLite publication and unchanged ordinary account admission. **Usable alone:** all shipped `provider check` variants now share the durable lifecycle; the setup UI can consume the completed interface.

Research → Plan → Implement gates apply to each slice; a contradictory finding stops that slice for a plan update. Formatting, Clippy and affected tests run during implementation, full workspace checks in CI. This plan revision runs documentation checks only.

## Exit criteria

- Every CLI check variant and the callable UI interface enforce acknowledged intent before each probe, validated outcome before continuation and no effects during replay.
- Store/record migrations, exact fixtures, crash/uncertainty scenarios and safe output prove the transitions above; no fake Session/Run or legacy runtime reader exists.
- Specs own the implemented contracts, including correction of the stale history-reset statement. Live paid-provider, native-store and human checks remain in [NEXT_STEPS](../../NEXT_STEPS.md#beta-2-and-broader-release); synthetic evidence is not live support.
- After C2, unblock setup S1; remove this plan/index row and update its dependent link under the [planning lifecycle](../README.md#lifecycle).
