# Automatic state migrations

## Metadata

- Status: `Draft`
- Owner: Fernando Mirabile
- Last updated: 2026-10-21

## Outcome and scope

Feature: an Arany build opens state written by an older build without manual reset. Supported history is upgraded automatically. History too old to upgrade honestly is archived intact, and a fresh store opens with a notice. Accounts and settings are never lost. From the moment this lands, every stored-format version bump ships its migration, so pre-beta dogfooding and future releases keep their history.

It covers every persisted format: the Event journal (`events.sqlite3`), `checkpoints.sqlite`, and the private JSON records in the StateRoot (preferences, accounts, folder trust). Excluded: the native-evidence tables, an expiring cache keyed by `*_CHECK_VERSION` that a bump correctly invalidates and re-checks; and the external serve/ACP/JSONL `event_version` contract, which stays with its owning specs.

## Research

[State migrations research](../../docs/research/state-migrations-2026-10.md) holds the inventory, churn, constraints and peer practice, with citations. The decision-relevant facts:

- About 45 Event kinds carry their own payload version (`payload_version()` in `crates/arany/src/session/events.rs`), and decoding rejects any other version. The journal schema is `user_version = 4` (`store/journal.rs`); `checkpoints.sqlite` is 1 (`store/state/checkpoints.rs`). The StateRoot holds 12 JSON record kinds (`RecordKind`, `store/state.rs`).
- Churn: about 1 in 8 PRs since 2026-10-07 bumped an Event version. Most added a field; some (`ToolFinished`, `RunStarted`) added facts that cannot be reconstructed honestly, so old history cannot be fully upgraded without inventing data.
- There is no hash chain over Events. Fork boundaries store a `prefix_digest` (`session/lineage.rs`) over re-serialized payloads, so any rewrite must recompute it; checkpoint blob references and compaction `content_digest` must stay valid.
- Today an incompatible store gives `state storage failed`, or a confirmed "Reset history" that deletes the journal (terminal.md History recovery; exclusive lease and identity checks already exist).
- Peers: Codex keeps rollout files readable through serde defaults and retained legacy variants, additive-only, with no explicit version. Event-sourcing practice uses upcasters or copy-and-transform migrations, and archive as the fallback.
- Rules to change: AGENTS.md global rule 5, which forbids migrations, and the "no migration" statements in execution.md, goals.md, terminal.md and credentials.md.

## Contract and scenarios

- Owning specs: execution.md (persistence and recovery), terminal.md (startup history recovery), credentials.md (account records), and session.md where replay validation is described. AGENTS.md rule 5 changes accordingly.
- Successful paths:
  - A journal at the current version opens unchanged.
  - A journal one or more migratable versions behind is upgraded in a copy, validated by strict replay, swapped atomically, and the old file is kept as a private backup. Every Session, Goal and fork digest still validates.
  - A JSON record behind is upgraded in its atomic pending replacement.
- Archive path: history older than the migration floor, or failing any migration step, is moved intact to `archive/history-<UTC timestamp>/` (events.sqlite3, its sidecars and checkpoints.sqlite). A fresh store opens with a visible notice naming the archive path. Accounts, settings and trust are untouched.
- Failure and recovery:
  - A crash at any point leaves either the original or the completed upgrade, never a mix.
  - A busy store (another Arany) is retried, not archived.
  - An unsafe or inaccessible state dir still fails closed with its typed cause.
  - Nothing is ever deleted silently.
- Evidence owners: store migration tests in `crates/arany/src/store`, startup PTY rows in `crates/arany-cli/tests/session_run/setup.rs`, and a version-bump guard test (below).

## Chosen direction

The hybrid: archive as the safety net, plus a copy-transform migration framework that every future bump must use.

- Runtime keeps one current format. Decoders and the reducer never see old shapes, so strict replay and "one current internal format at runtime" hold.
- Migration steps are JSON-level `Value -> Value` transforms keyed by (kind, from-version). They run only in a `store::migrate` module, on open, under the existing exclusive lease.
- Floor = the current versions when slice 2 lands. Anything older is archived (slice 1), so none of today's ~19 historical bumps need transforms.
- A guard test iterates `payload_version()`, the schema versions and the record versions, and fails when a version is above the floor without a registered step and a fixture.
- OS impact: none beyond existing atomic rename and private-directory rules on Linux and macOS.

## Security and resource model

- Migrated or archived data stays data. A transform never adds authority, grants or trust, and never reads repository content.
- Archives inherit the private StateRoot permissions.
- Bounds: migration works within the existing journal size limits and one copy's free space. If space is short, the result is a typed `StorageFull` with nothing changed.
- Account and credential records only migrate forward through explicit steps. On failure they keep today's preserve-and-reset behavior.

## Risks and mitigations

- A transform fabricating facts → a step may only add a field with a value the old version proves (or an explicit `unknown`), and otherwise refuses, which archives. Fixtures pin each step.
- A digest mismatch after rewrite → recompute fork `prefix_digest`s in the same transaction, then strict-replay the whole result before the swap.
- A bump without a step → the guard test fails CI.

## Vertical slices

1. **Automatic archive at startup.** An incompatible or damaged history at attached startup (and in `exec`/`serve`/`acp`) is archived intact under `archive/history-<ts>/`, a fresh store opens, and a notice names the archive. Accounts and settings are kept. This replaces the manual "Reset history" path for incompatible history (damaged or busy keep their current handling), updates terminal.md and the AGENTS.md rule 5 text, and adds tests that an old-version journal opens fresh with the archive present and an unsafe dir still fails closed. Depends on the startup history recovery fix (`fix/startup-history-recovery`).
2. **Journal migration framework and guard.** Adds the `store::migrate` registry, the copy → transform → recompute digests → strict replay → atomic swap with backup, a `MigrationRecorded` fact, the floor at current versions, and the guard test. It also adds one real example step with a fixture, the next Event bump, to prove the path end to end. Updates execution.md and session.md.
3. **Checkpoint store migration.** `checkpoints.sqlite` schema steps through the same framework, keeping blob references valid. Its tests are an old schema upgraded with rewind still working.
4. **JSON record migration.** The registry covers the `RecordKind` records through their atomic pending replacement, starting with ui-preferences v3 → v4 (fixture already in `controls.rs`) and the native account schema. Updates terminal.md, credentials.md and goals.md.

Each slice is one PR and usable on its own: after slice 1 nobody is blocked by an old store; after slice 2 Session history survives every future Event bump.

## Open questions

- Owner: accept that history older than the floor (all history written before slice 2 lands) is archived, not upgraded? The research shows several past bumps cannot be reconstructed honestly.

## Exit criteria

- An old-format state dir from any pre-floor build opens without manual steps, either archived or migrated, on Linux and macOS (CI fixtures; native owner check on the owner's Mac).
- The guard test blocks a version bump without its migration.
- AGENTS.md rule 5 and the owning specs describe the new contract.
- The plan closes under the [planning lifecycle](../README.md#lifecycle).
