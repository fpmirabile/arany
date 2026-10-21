# Automatic state migrations

## Metadata

- Status: `In progress`
- Owner: Fernando Mirabile
- Last updated: 2026-10-21

## Outcome and scope

Feature: an Arany build opens state written by an older build without manual reset. Supported history is upgraded automatically. History too old to upgrade honestly is archived intact, and a fresh store opens with a notice. Accounts and settings are never lost. From the moment this lands, every stored-format version bump ships its migration, so pre-beta dogfooding and future releases keep their history.

It covers every persisted format: the Event journal (`events.sqlite3`), `checkpoints.sqlite`, and the private JSON records in the StateRoot (preferences, accounts, folder trust). Excluded: the native-evidence tables, an expiring cache keyed by `*_CHECK_VERSION` that a bump correctly invalidates and re-checks; and the external serve/ACP/JSONL `event_version` contract, which stays with its owning specs.

## Research

[State migrations research](../../docs/research/state-migrations-2026-10.md) owns the persisted-format inventory, commit-based churn measurement, digest and storage constraints, and peer evidence and its limits.

## Contract and scenarios

- Owning specs: execution.md (persistence and recovery), terminal.md (startup history recovery), credentials.md (account records), and session.md where replay validation is described. AGENTS.md rule 5 changes accordingly.
- Successful paths:
  - A journal at the current version opens unchanged.
  - A journal one or more migratable versions behind is upgraded in a copy, validated by strict replay, swapped atomically, and the old file is kept as a private backup. Every Session, Goal and fork digest still validates.
  - A JSON record behind is upgraded in its atomic pending replacement.
- Archive path: history older than the migration floor, or failing any migration step, is moved intact to `archive/history-<UTC timestamp>/` (events.sqlite3, its sidecars and checkpoints.sqlite). A fresh store opens with a visible notice naming the archive path. Accounts, settings and trust are untouched.
- Failure and recovery:
  - Fault injection before and after each migration transaction commit, phase-marker publication, file/directory sync, swap and marker removal, followed by repeated startup, proves the [swap recovery protocol](#chosen-direction): either the original opens without a fact for this migration or the validated upgrade opens with exactly one matching fact. Ambiguous identities fail closed without archiving either candidate.
  - A backup reopens independently with every committed record, including a fixture with uncheckpointed WAL records; unsupported SQLite state is preserved and refused.
  - A busy store (another Arany) is retried, not archived.
  - An unsafe or inaccessible state dir still fails closed with its typed cause.
  - Nothing is ever deleted silently.
- Evidence owners: store migration tests in `crates/arany/src/store`, startup PTY rows in `crates/arany-cli/tests/session_run/setup.rs`, and a version-bump guard test (below).

## Chosen direction

The hybrid: archive as the safety net, plus a copy-transform migration framework that every future bump must use.

- Runtime keeps one current format. JSON-level `Value -> Value` record and Event transforms keyed by (kind, from-version) run only in the bounded migration path under the exclusive lease, then pass current records to the owning decoders and reducer. Legacy handling never enters ordinary decoders. Store orchestration stays in `store::migrate`; JSON shape knowledge stays with each record owner.
- Journal floor = the current versions when slice 2 lands; [D-22](../../docs/research/next-step-decision-register.md#d-22--pre-beta-history-below-the-migration-floor) owns the pre-floor archive trade-off. Slice 4 declares each JSON record floor, including ui-preferences v3 for its initial v3 → v4 step. Floors stay fixed across subsequent bumps.
- The guard test enumerates every in-scope version family: each Event kind's `payload_version()`, both SQLite `user_version`s, and every persisted JSON record version. For each family it requires a registered transform and fixture for every adjacent source → target edge from its declared floor through current, retaining older edges after later bumps. Multi-step fixtures also prove migration from the floor to current. Slice 2 introduces the guard; slices 3 and 4 extend its registry and fixtures as their families become migratable.
- SQLite backups use the online backup API under the exclusive lease with other owners quiesced, producing a timestamped private, self-contained snapshot. SQLite reads any admitted WAL records; raw copies of `events.sqlite3`, `-wal` or `-shm` are not backups. Before replacement, SQLite completes recovery/checkpointing and closes all source connections; unexpected or unsafe sidecars fail closed. Backup and staged output use DELETE mode and close without live sidecars; source sidecars are never attached to the replacement.
- Swap recovery uses a private, atomically replaced phase marker, syncing its file and parent directory at each publication. `Preparing` records a migration ID, source identity, pinned versions and backup/staging paths before copying. The staged database commits a `MigrationRecorded` row in its migration table with that ID in the transaction finalizing its transformed data and recomputed digests, then passes whole-history strict replay. After closing and syncing the backup and staged database and their parent directories, publish `Ready` with their verified identities/digests and the source identity/digest. Atomically replace the source, sync its directory, then remove the marker and sync again; retain the backup. The migration fact is installed with the database, never appended after the rename.
- Before ordinary startup, recovery holds the exclusive lease. With `Preparing`, verify the original identity, discard the marker's incomplete staging/backup artifacts, clear the marker with directory sync and retain the original. With `Ready` and the original still present, verify its digest and abandon staging; with the matching upgraded database present, verify its digest and migration ID and finish directory sync. Both `Ready` cases validate the retained backup before marker cleanup; cleanup tolerates already-discarded staging. Missing required artifacts or ambiguous identities fail closed and preserve the artifacts. Recovery never re-executes journaled effects.
- OS impact: the same protocol requires durable file and directory sync and atomic replacement in the admitted StateRoot on Linux and macOS; platform CI fixtures verify recovery, while native support evidence remains separate.

## Security and resource model

- Migrated or archived data stays data. A transform never adds authority, grants or trust, and never reads repository content.
- Archives inherit the private StateRoot permissions.
- Bounds: migration keeps the existing per-store size limits and budgets space for both the backup and staged database, SQLite sidecars and bounded phase metadata. If space is short, the result is a typed `StorageFull` with the original preserved.
- Account and credential records only migrate non-secret shapes forward through explicit steps. On failure they keep today's preserve-and-explicit-reset behavior.

## Risks and mitigations

- A transform fabricating facts → a step may only add a field with a value the old version proves (or an explicit `unknown`), and otherwise refuses, which archives. Fixtures pin each step.
- A digest mismatch after rewrite → recompute fork `prefix_digest`s in the same transaction, then strict-replay the whole result before the swap.
- A bump without a step → the guard test fails CI.

## Vertical slices

1. **Automatic archive at startup.** An incompatible or damaged history at attached startup (and in `exec`/`serve`/`acp`) is archived intact under `archive/history-<ts>/`, a fresh store opens, and a notice names the archive. Accounts and settings are kept. This replaces the manual "Reset history" path for incompatible history (damaged or busy keep their current handling), updates terminal.md and the AGENTS.md rule 5 text, and adds tests that an old-version journal opens fresh with the archive present and an unsafe dir still fails closed. Implemented on `fix/startup-history-recovery`.
2. **Journal migration framework and guard.** Implements the journal protocol and guard in [Chosen direction](#chosen-direction), with the backup and crash scenarios in [Contract and scenarios](#contract-and-scenarios). It also adds one real example step with a fixture, the next Event bump, to prove the path end to end. Updates execution.md and session.md.
3. **Checkpoint store migration.** `checkpoints.sqlite` schema steps through the same framework, keeping blob references valid. Its tests are an old schema upgraded with rewind still working.
4. **JSON record migration.** Extends the migration registry and guard to the `RecordKind` records through their atomic pending replacement, following [Chosen direction](#chosen-direction) and the [security model](#security-and-resource-model). Starts with the [existing ui-preferences fixture](../../docs/research/state-migrations-2026-10.md#3-arany-constraints) and the native account schema. Updates terminal.md, credentials.md and goals.md.

Each slice is one PR and usable on its own: after slice 1 nobody is blocked by an old store; after slice 2 Session history survives every future Event bump.

## Exit criteria

- An old-format state dir from any pre-floor build opens without manual steps, either archived or migrated, on Linux and macOS (CI fixtures; native owner check on the owner's Mac).
- The guard test blocks a version bump without its migration.
- AGENTS.md rule 5 and the owning specs describe the new contract.
- The plan closes under the [planning lifecycle](../README.md#lifecycle).
