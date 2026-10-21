# State migrations research

- Question: how can Arany preserve stored state across format changes without inventing replay facts?
- Source-check date: 2026-10-21. Repository inventory is pinned to `40be2a7a`; the Codex revision and evidence limits are in [peer practice](#4-peer-practice).
- Related decisions: the [active plan](../../planning/state-migrations/README.md#chosen-direction) owns the design; [D-22](./next-step-decision-register.md#d-22--pre-beta-history-below-the-migration-floor) owns the accepted archive trade-off.
- Scope: persisted-format inventory, churn, digest and storage constraints, and peer evidence; no implementation or native-platform verification claim.

## 1. Inventory (verified in repo @ main 40be2a7a)

| Format | Owner file | Version today | Rejection |
|---|---|---|---|
| events.sqlite3 SQLite schema | crates/arany/src/store/journal.rs (SCHEMA_SQL, `PRAGMA user_version=4`, check ~L162 `version != 4 && !fresh`) | 4 | StoreError::InvalidHistory, no change to data |
| Event payloads | crates/arany/src/session/events.rs `payload_version(kind)` (~L133); `Event::decode` L1047 `payload_version(kind) != Some(version)` -> ReplayError::UnknownEvent | per kind: ToolFinished 19, RunStarted 17, ProviderCallRecorded 13, ToolStarted 13, AgentSpawned 11, AgentFinished 10, RunFinished 9, HookFinished 8, MessageAccepted 7, GoalStarted/Finished 7, ContextCompacted 7, GoalProviderCallReserved/GoalCompactionCallReserved 7, 11 session/goal kinds 6, ToolPermissionDecided 4, HookStarted 3, WorkerHandoverRecorded 2, rest 1 (~45 kinds) | exact match only; payload parse is strict (missing fields reject) |
| compaction record | crates/arany/src/session/events/records.rs L482 prompt_version 1, schema_version 1, compiler_version 3 | | reject |
| native-evidence tables (provider_evidence, native_provider_evidence) | same events.sqlite3, store/evidence.rs; keyed by 32-byte fingerprint/profile digest | covered by schema 4; fingerprint embeds CHECK_VERSION (native_check.rs v31, custom/check.rs v30, chatgpt/account.rs MODEL_CHECK_VERSION v29, ACCOUNT_ADMISSION_VERSION v31) | expiring cache; a bump just invalidates it (re-check), no data loss |
| checkpoints.sqlite | crates/arany/src/store/state/checkpoints.rs L13/L261 | user_version 1 | StoreError::CheckpointUnavailable |
| ui-preferences.json | crates/arany-cli/src/cli/attached/controls.rs L20 UI_PREFERENCES_VERSION (RecordVersion L31, decode_settings L346) | 4 | "unsupported settings", chat runs on defaults; exec/serve refuse |
| model-preferences.json | arany-cli/src/cli/attached/models.rs L112 | 1 | reject |
| account-credentials.json | arany-cli/src/accounts/credentials.rs L81/L284/L345 | schema 2 (native; schema 1 rejected), index schema 1 | reject, preserve; explicit reset |
| chatgpt-accounts.json, chatgpt-registration.json | arany-cli/src/accounts/chatgpt/account.rs (schema 1), registration.rs L11 (schema 1); consent WARNING_VERSION 4 (consent.rs) | 1 | reject |
| held-account-marker.json | not found as such by grep in crates (not verified; maybe named differently or removed) | ? | |
| provider-profiles.json | arany/src/provider/custom.rs L111,147 | version 1, capability_evidence_version 2 | reject |
| provider-roster.json / provider-routing.json | arany-cli/src/cli/provider/roster.rs L84, routing.rs L100 | 1 / 1 | reject (user-authored) |
| workspace-permissions.json | store/state.rs RecordKind::WorkspacePermissions; run.rs fixtures use version 2 | 2 | reject |
| tools.json, permissions.json | user/workspace-authored | 1 | reject |
| unguarded-commands.json | arany/src/tools/unguarded_rules.rs | 1 | reject |
| guard reservation (transient) | [tools/guard/admission.rs](../../crates/arany/src/tools/guard/admission.rs) `RESERVATION_VERSION` | 2 (Linux) / 1 (macOS) | `ToolError::ProtectionUnavailable` on version mismatch |

RecordKind enum (store/state.rs L74) lists 12 JSON record kinds, all in one StateRoot with pending-file atomic replace. Protocol: JSONL/serve emit `event_version` straight from payload_version (protocol.rs L32), so serve clients see version bumps too (external contract).
Total: ~45 Event kinds across ~19 distinct version numbers, 2 SQLite DBs, ~12 JSON records, 4 check-version strings. Existing user-facing recovery: attached startup "Reset history" (discard, terminal.md ~L207-229) and credential/model resets.

## 2. Churn

- 201 commits since 2026-10-07 (235 total in repo, first 2026-09-29); 29 commits matched changes to the `payload_version` table (`git log 40be2a7a --since=2026-10-07 --format=%H -G '=> Some\([0-9]+\)' -- crates/arany/src/session/events.rs`), about 14% of commits. 30 commits touched events.rs. Highest counters: ToolFinished 19, RunStarted 17, ProviderCall 13: each ~1 bump per active day. Bumps are mostly "required nullable field added" (execution.md L157/L180), which is the easiest upcast (insert null/default), but several are semantic (ToolFinished checkpoint outcome, before/after digests; RunStarted roster/definition pins) where the old value cannot be reconstructed honestly.
- Cumulative cost of full upcasting: ~19 version steps across ~11 evolving kinds already, +~1 step/day at current rate.

## 3. Arany constraints

- Hash/chain: no hash chain over the journal. [PrefixHasher](../../crates/arany/src/session/lineage.rs) hashes "arany-session-prefix-v1", sequence, Session and Run IDs, kind, **re-serialized payload text** and creation time. SessionForked records the resulting `prefix_digest`, which [journal loading](../../crates/arany/src/store/journal.rs) and replay validate. Changing a hashed payload changes the expected prefix digest; fork sources and boundaries live in the same database. Checkpoint blob references and compaction `content_digest` are additional persisted integrity checks.
- Strict replay: [Event::decode](../../crates/arany/src/session/events.rs) rejects unknown versions. [Execution](../specs/execution.md#admission-persistence-and-recovery), [Goals](../specs/goals.md#lifecycle-budgets-and-recovery), [terminal recovery](../specs/terminal.md#setup-and-account-choices) and [AGENTS.md rule 5](../../AGENTS.md#global-rules) currently exclude migrations. Replay does not execute effects; missing facts cannot establish past outcomes.
- Journal-first durability is an [existing guarantee](../../AGENTS.md#non-negotiable-guarantees). SQLite commits in [journal.rs](../../crates/arany/src/store/journal.rs) and filesystem replacement in [state.rs](../../crates/arany/src/store/state.rs) are separate operations; there is no existing migration transaction spanning them.
- State admission uses private directory capabilities, bounded reads and pending-file replacement in [state.rs](../../crates/arany/src/store/state.rs). History reset holds an exclusive history lock and verifies file identities before mutation. Accounts use the existing private record API; unsupported credentials are preserved pending explicit reset ([credential contract](../specs/credentials.md#limits-and-record-admission)).
- Both [journal.rs](../../crates/arany/src/store/journal.rs) and [checkpoints.rs](../../crates/arany/src/store/state/checkpoints.rs) require `journal_mode=DELETE`. SQLite's [online backup API](https://www.sqlite.org/backup.html) produces a database snapshot; its [WAL documentation](https://www.sqlite.org/wal.html#the_wal_file) explains that separating the database from its WAL can lose committed transactions. This WAL risk is conditional, not evidence that Arany currently writes in WAL mode.
- Checkpoint pre-image blobs in [checkpoints.sqlite](../../crates/arany/src/store/state/checkpoints.rs) are content-addressed and referenced by ToolFinished checkpoint outcomes.
- The complete ui-preferences v3 fixture in [controls.rs](../../crates/arany-cli/src/cli/attached/controls.rs) is currently rejected without conversion or overwrite. Its v4 record adds mouse and copy-on-select settings; the fixture supplies evidence for the initial record migration named in the plan.
- Native-evidence entries are keyed by versioned fingerprints; changing the check version invalidates the cache ([evidence.rs](../../crates/arany/src/store/evidence.rs)).

## 4. Peer practice

- Codex CLI at revision `70b3bb6d8814ab3f2045238b4ae14c43da99783b`: the [protocol source](https://github.com/openai/codex/blob/70b3bb6d8814ab3f2045238b4ae14c43da99783b/codex-rs/protocol/src/protocol.rs#L1513-L1520) retains `ThreadRolledBack` for replay and accepts `turn_started` as an alias for `task_started`. It also [defaults an absent `ItemCompletedEvent.completed_at_ms`](https://github.com/openai/codex/blob/70b3bb6d8814ab3f2045238b4ae14c43da99783b/codex-rs/protocol/src/protocol.rs#L2063-L2079) to keep older persisted events deserializable. These are observed compatibility mechanisms, not evidence that all rollout evolution is additive or that files are never rewritten; rollout storage and migration paths were not investigated.
- Unresolved: Claude Code transcript compatibility and general event-sourcing recommendations have no verified primary-source evidence in this investigation. They are excluded from the recommendation.
