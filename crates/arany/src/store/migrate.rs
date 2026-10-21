use super::journal::{SCHEMA_SQL, SCHEMA_VERSION, open_database};
use super::replay::{ROWS_AFTER_SQL, RawRow, raw_row, resolve_view, validate_history};
use super::state::{
    HISTORY_FILES, Identity, archive_name, check_private_record_file, identity_of, nonblocking,
    private_subdirectory, publish, rename, step, sync, valid_archive_name,
};
use super::{DATABASE_FILE, MAX_DATABASE_BYTES, StateRoot, StoreError, event_payload_limit};
use crate::session::{Event, PrefixHasher, PrefixRow, SessionId, payload_version};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, config::DbConfig};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{self, Read};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
mod tests;

const DIR: &str = "migrations";
const MARKER: &str = "phase.json";
const MARKER_TEMP: &str = "phase.tmp";
const STAGED: &str = "staged.sqlite3";
const MAX_MARKER_BYTES: usize = 4096;
const STAGING_HEADROOM_BYTES: u64 = 4 * 1024 * 1024;
// Cumulative events loaded while replaying every Session, ancestors included.
const MAX_VALIDATION_EVENTS: usize = 1 << 20;

pub(super) struct SchemaStep {
    pub(super) from: i64,
    // The exact schema at `from`; any other object (trigger, view, table, index) refuses.
    pub(super) source: &'static str,
    pub(super) sql: &'static str,
}

pub(super) struct EventStep {
    pub(super) kind: &'static str,
    pub(super) from: i64,
    // Returns `None` when the old payload cannot prove a required fact.
    pub(super) transform: fn(Value) -> Option<Value>,
    // Serializes a stored source-version payload exactly as that version's replay hashed it.
    pub(super) canonical: fn(&str) -> Option<String>,
}

pub(super) struct Registry<'a> {
    pub(super) schema_floor: i64,
    pub(super) schema_steps: &'a [SchemaStep],
    pub(super) event_floor: &'a [(&'a str, i64)],
    pub(super) event_steps: &'a [EventStep],
}

pub(super) const REGISTRY: Registry<'static> = Registry {
    schema_floor: 4,
    schema_steps: &[SchemaStep {
        from: 4,
        source: "CREATE TABLE events (
                sequence INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                run_id TEXT,
                agent_run_id TEXT,
                kind TEXT NOT NULL,
                event_version INTEGER NOT NULL CHECK (event_version >= 1),
                payload TEXT NOT NULL CHECK (json_valid(payload)),
                created_at_ms INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX events_by_session ON events (session_id, sequence);
            CREATE INDEX events_by_run ON events (run_id, sequence) WHERE run_id IS NOT NULL;
            CREATE TABLE provider_evidence (
                profile_name TEXT PRIMARY KEY,
                profile_digest BLOB NOT NULL CHECK(length(profile_digest) = 32),
                addresses TEXT NOT NULL CHECK(json_valid(addresses)),
                checked_at_ms INTEGER NOT NULL,
                expires_at_ms INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE native_provider_evidence (
                fingerprint BLOB PRIMARY KEY CHECK(length(fingerprint) = 32),
                checked_at_ms INTEGER NOT NULL,
                expires_at_ms INTEGER NOT NULL
            ) STRICT;",
        sql: "CREATE TABLE migrations (
                id TEXT PRIMARY KEY CHECK (length(id) = 36),
                from_schema INTEGER NOT NULL,
                to_schema INTEGER NOT NULL,
                rewritten_events INTEGER NOT NULL CHECK (rewritten_events >= 0),
                backup TEXT NOT NULL,
                recorded_at_ms INTEGER NOT NULL
            ) STRICT;",
    }],
    event_floor: &EVENT_FLOOR,
    event_steps: &[],
};

// Payload versions when migrations landed; older history is archived, never upgraded.
const EVENT_FLOOR: [(&str, i64); 41] = [
    ("SessionStarted", 6),
    ("SessionDefaultChanged", 6),
    ("SessionForked", 6),
    ("SessionRenamed", 6),
    ("GoalRunLinked", 6),
    ("GoalResumed", 6),
    ("GoalPlanContractRecorded", 6),
    ("GoalContextCompacted", 6),
    ("GoalSteeringNoteRecorded", 6),
    ("AssistantDisplayRecorded", 6),
    ("MessageCommitted", 6),
    ("MessageAccepted", 7),
    ("AgentSpawned", 11),
    ("AgentFinished", 10),
    ("ProviderCallRecorded", 13),
    ("WorkerDataChunk", 1),
    ("WorkerDataSealed", 1),
    ("WorkerStopObserved", 1),
    ("WorkerIntegrationRecorded", 1),
    ("WorkerHandoverDiscarded", 1),
    ("WorkerLeaseReleased", 1),
    ("WorkerVerificationRecorded", 1),
    ("WorkerHandoverRecorded", 2),
    ("ToolResultsCleared", 1),
    ("GoalStarted", 7),
    ("GoalFinished", 7),
    ("SupervisorVerdictRecorded", 1),
    ("WorkspaceRewindStarted", 1),
    ("WorkspaceRewindFinished", 1),
    ("ProviderFallbackUsed", 1),
    ("ProviderRetryScheduled", 1),
    ("ToolPermissionDecided", 4),
    ("HookStarted", 3),
    ("HookFinished", 8),
    ("ToolFinished", 19),
    ("RunStarted", 17),
    ("RunFinished", 9),
    ("GoalProviderCallReserved", 7),
    ("GoalCompactionCallReserved", 7),
    ("ToolStarted", 13),
    ("ContextCompacted", 7),
];

impl Registry<'_> {
    fn event_floor(&self, kind: &str) -> Option<i64> {
        self.event_floor
            .iter()
            .find(|(name, _)| *name == kind)
            .map(|(_, floor)| *floor)
    }

    fn event_step(&self, kind: &str, from: i64) -> Option<&EventStep> {
        self.event_steps
            .iter()
            .find(|step| step.kind == kind && step.from == from)
    }

    fn schema_step(&self, from: i64) -> Option<&SchemaStep> {
        self.schema_steps.iter().find(|step| step.from == from)
    }

    fn expected_schema(&self, version: i64) -> Option<&'static str> {
        if version == SCHEMA_VERSION {
            Some(SCHEMA_SQL)
        } else {
            self.schema_step(version).map(|step| step.source)
        }
    }

    // `None` for a current payload; prefix digests of older versions hash this canonical text.
    fn historical_payload(
        &self,
        kind: &str,
        version: i64,
        payload: &str,
    ) -> Result<Option<String>, StoreError> {
        if payload_version(kind) == Some(version) {
            return Ok(None);
        }
        let step = self
            .event_step(kind, version)
            .ok_or(StoreError::InvalidHistory)?;
        (step.canonical)(payload)
            .map(Some)
            .ok_or(StoreError::InvalidHistory)
    }

    // `None` keeps the stored payload; an unsupported version or a refused step rejects.
    fn upgrade_payload(
        &self,
        kind: &str,
        version: i64,
        payload: &str,
    ) -> Result<Option<String>, StoreError> {
        let current = payload_version(kind).ok_or(StoreError::InvalidHistory)?;
        if version == current {
            return Ok(None);
        }
        if self.event_floor(kind).is_none_or(|floor| version < floor) || version > current {
            return Err(StoreError::InvalidHistory);
        }
        let mut value: Value =
            serde_json::from_str(payload).map_err(|_| StoreError::InvalidHistory)?;
        for from in version..current {
            let step = self
                .event_step(kind, from)
                .ok_or(StoreError::InvalidHistory)?;
            value = (step.transform)(value).ok_or(StoreError::InvalidHistory)?;
        }
        serde_json::to_string(&value)
            .map(Some)
            .map_err(|_| StoreError::InvalidHistory)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    id: String,
    source: Identity,
    from_schema: i64,
    to_schema: i64,
    backup: String,
    phase: Phase,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
enum Phase {
    Preparing,
    Ready {
        source_digest: [u8; 32],
        backup_digest: [u8; 32],
        staged: Identity,
        staged_digest: [u8; 32],
    },
}

pub(super) fn pending(root: &StateRoot) -> Result<bool, StoreError> {
    match root.dir.open_dir_nofollow(DIR) {
        Ok(dir) => Ok(identity_of(&dir, MARKER)?.is_some()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

// Checked under the shared lease, so no migration can start or crash between the check and the open.
pub(super) fn lease_after_recovery(
    root: &StateRoot,
    writable: bool,
) -> Result<super::state::HistoryLock, StoreError> {
    for _ in 0..3 {
        let shared = root.lock_history(false)?;
        if !writable || !pending(root)? {
            return Ok(shared);
        }
        drop(shared);
        let _exclusive = root.lock_history(true)?;
        recover(root)?;
    }
    Err(StoreError::HistoryChanged)
}

// The caller holds the exclusive history lock.
pub(super) fn recover(root: &StateRoot) -> Result<(), StoreError> {
    let Some((dir, marker)) = read_marker(root)? else {
        return Ok(());
    };
    let current = identity_of(&root.dir, DATABASE_FILE)?;
    match &marker.phase {
        Phase::Preparing => {
            if current != Some(marker.source) {
                return Err(StoreError::MigrationConflict);
            }
            discard(&dir, STAGED)?;
            discard(&dir, &marker.backup)?;
        }
        Phase::Ready {
            source_digest,
            backup_digest,
            staged,
            staged_digest,
        } => {
            let original = current == Some(marker.source);
            let staging = identity_of(&dir, STAGED)?;
            let matches = if original {
                digest(&root.dir, DATABASE_FILE)? == Some(*source_digest)
                    && (staging.is_none()
                        || staging == Some(*staged)
                            && digest(&dir, STAGED)? == Some(*staged_digest))
            } else {
                staging.is_none()
                    && current == Some(*staged)
                    && digest(&root.dir, DATABASE_FILE)? == Some(*staged_digest)
                    && recorded(&root.path().join(DATABASE_FILE), &marker.id)?
            };
            if !matches || digest(&dir, &marker.backup)? != Some(*backup_digest) {
                return Err(StoreError::MigrationConflict);
            }
            // The original stays live, so this attempt's copies are discarded; a retry makes its own.
            if original {
                discard(&dir, STAGED)?;
                discard(&dir, &marker.backup)?;
            }
        }
    }
    sync(&root.dir)?;
    sync(&dir)?;
    step()?;
    dir.remove_file(MARKER)?;
    sync(&dir)?;
    if matches!(marker.phase, Phase::Ready { .. }) && current != Some(marker.source) {
        keep_only(&dir, &marker.backup)?;
    }
    Ok(())
}

// An interrupted copy can leave a rollback journal; it goes before its database.
fn discard(dir: &Dir, name: &str) -> Result<(), StoreError> {
    remove_if_present(dir, &format!("{name}-journal"))?;
    remove_if_present(dir, name)
}

// `migrations/` holds at most the latest completed migration's backup; older backups are removed.
fn keep_only(dir: &Dir, backup: &str) -> Result<(), StoreError> {
    let mut older = Vec::new();
    for entry in dir.entries()? {
        let name = entry?.file_name();
        let name = name.to_str().ok_or(StoreError::InvalidStateDirectory)?;
        if name.starts_with("history-") && name != backup {
            older.push(name.to_owned());
        }
    }
    for name in &older {
        remove_if_present(dir, name)?;
    }
    sync(dir)
}

// The caller holds the exclusive history lock and has found the history incompatible.
pub(super) fn upgrade(root: &StateRoot, registry: &Registry<'_>) -> Result<bool, StoreError> {
    let source_path = root.path().join(DATABASE_FILE);
    let Some(from_schema) = migratable(&open_source(&source_path, true)?, registry)? else {
        return Ok(false);
    };
    drop(open_source(&source_path, false)?);
    for sidecar in &HISTORY_FILES[..3] {
        if identity_of(&root.dir, sidecar)?.is_some() {
            return Err(StoreError::InvalidStateDirectory);
        }
    }
    let source = identity_of(&root.dir, DATABASE_FILE)?.ok_or(StoreError::InvalidHistory)?;
    let dir = private_subdirectory(&root.dir, DIR)?;
    sync(&root.dir)?;
    reserve_space(&dir, source.2)?;
    let id = uuid::Uuid::now_v7().to_string();
    let mut marker = Marker {
        backup: format!("{}-{id}.sqlite3", archive_name(now())),
        id,
        source,
        from_schema,
        to_schema: SCHEMA_VERSION,
        phase: Phase::Preparing,
    };
    if identity_of(&dir, &marker.backup)?.is_some() {
        return Err(StoreError::HistoryChanged);
    }
    write_marker(&dir, &marker)?;
    let staged = match stage(root, &dir, &marker, registry) {
        Ok(staged) => staged,
        Err(error) => {
            // A refused or failed copy leaves the original; discard only this attempt's files.
            let discarded = recover(root);
            return match error {
                StoreError::InvalidHistory
                | StoreError::InvalidSnapshot
                | StoreError::ReplayLimit
                    if discarded.is_ok() =>
                {
                    Ok(false)
                }
                error => Err(storage(error)),
            };
        }
    };
    if let Err(error) = swap(root, &dir, &mut marker, staged) {
        let _ = recover(root);
        return Err(storage(error));
    }
    step()?;
    dir.remove_file(MARKER)?;
    sync(&dir)?;
    keep_only(&dir, &marker.backup)?;
    Ok(true)
}

fn swap(
    root: &StateRoot,
    dir: &Dir,
    marker: &mut Marker,
    staged: Identity,
) -> Result<(), StoreError> {
    marker.phase = Phase::Ready {
        source_digest: digest(&root.dir, DATABASE_FILE)?.ok_or(StoreError::HistoryChanged)?,
        backup_digest: digest(dir, &marker.backup)?.ok_or(StoreError::HistoryChanged)?,
        staged,
        staged_digest: digest(dir, STAGED)?.ok_or(StoreError::HistoryChanged)?,
    };
    if identity_of(&root.dir, DATABASE_FILE)? != Some(marker.source) {
        return Err(StoreError::HistoryChanged);
    }
    write_marker(dir, marker)?;
    rename(dir, STAGED, &root.dir, DATABASE_FILE)?;
    sync(&root.dir)?;
    sync(dir)
}

fn stage(
    root: &StateRoot,
    dir: &Dir,
    marker: &Marker,
    registry: &Registry<'_>,
) -> Result<Identity, StoreError> {
    let migrations = root.path().join(DIR);
    let staged_path = migrations.join(STAGED);
    {
        let source = open_source(&root.path().join(DATABASE_FILE), true)?;
        for name in [marker.backup.as_str(), STAGED] {
            create_private(dir, name)?;
            // SQLite opens the target by name; a lossy path could name another file.
            let target = migrations.join(name);
            let target = target.to_str().ok_or(StoreError::InvalidStateDirectory)?;
            source.execute("VACUUM INTO ?1", [target])?;
        }
    }
    sync_file(dir, &marker.backup)?;
    {
        let mut staged = open_source(&staged_path, false)?;
        staged.execute_batch("PRAGMA synchronous=EXTRA;")?;
        let page_size: i64 = staged.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        staged.query_row(
            &format!("PRAGMA max_page_count={}", MAX_DATABASE_BYTES / page_size),
            [],
            |_| Ok(()),
        )?;
        if !exact_schema(&staged, registry, marker.from_schema)? {
            return Err(StoreError::InvalidHistory);
        }
        let transaction = staged.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for from in marker.from_schema..SCHEMA_VERSION {
            let schema_step = registry
                .schema_step(from)
                .ok_or(StoreError::InvalidHistory)?;
            transaction.execute_batch(schema_step.sql)?;
        }
        let rewritten = rewrite_events(&transaction, registry)?;
        transaction.execute(
            "INSERT INTO migrations (id, from_schema, to_schema, rewritten_events, backup, recorded_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                marker.id,
                marker.from_schema,
                marker.to_schema,
                rewritten,
                marker.backup,
                i64::try_from(
                    now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |elapsed| elapsed.as_millis())
                )
                .unwrap_or(i64::MAX),
            ],
        )?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        step()?;
        transaction.commit()?;
    }
    validate_all(&open_database(&staged_path, false)?, validation_limit())?;
    if identity_of(dir, &format!("{STAGED}-journal"))?.is_some() {
        return Err(StoreError::InvalidStateDirectory);
    }
    sync_file(dir, STAGED)?;
    sync(dir)?;
    identity_of(dir, STAGED)?.ok_or(StoreError::HistoryChanged)
}

type Pins = ([u8; 32], [u8; 32]);

#[derive(Default)]
struct Lane {
    historical: PrefixHasher,
    upgraded: PrefixHasher,
}

// A recorded digest must match the historical prefix before it is re-pinned to the upgraded one.
fn repin(recorded: &mut [u8; 32], pins: Option<&Pins>) -> Result<(), StoreError> {
    match pins {
        Some((historical, upgraded)) if recorded == historical => {
            *recorded = *upgraded;
            Ok(())
        }
        _ => Err(StoreError::InvalidHistory),
    }
}

fn rewrite_events(
    transaction: &rusqlite::Transaction<'_>,
    registry: &Registry<'_>,
) -> Result<i64, StoreError> {
    let mut lanes = HashMap::<SessionId, Lane>::new();
    // Whole-Session prefix digests after fork and compaction boundaries and after each ToolFinished.
    let mut boundaries = HashMap::<(SessionId, u64), Pins>::new();
    let mut statement = transaction.prepare(ROWS_AFTER_SQL)?;
    let (mut after, mut rewritten) = (0_i64, 0_i64);
    loop {
        let mut rows = Vec::new();
        let mut query = statement.query([after])?;
        while let Some(row) = query.next()? {
            rows.push(raw_row(row)?);
        }
        drop(query);
        let Some(last) = rows.last() else {
            return Ok(rewritten);
        };
        after = last.sequence;
        for row in rows {
            let (sequence, original) = (row.sequence, (row.version, row.payload.clone()));
            let historical = registry.historical_payload(&row.kind, row.version, &row.payload)?;
            let mut envelope = upgraded_row(registry, row)?.decode()?;
            let historical = match historical {
                Some(payload) => payload,
                None => envelope.event.payload()?,
            };
            let session = envelope.session_id;
            let lane = lanes.entry(session).or_default();
            let head = (lane.historical.digest(), lane.upgraded.digest());
            match &mut envelope.event {
                Event::SessionForked {
                    source_session_id,
                    source_sequence,
                    prefix_digest,
                    ..
                } if *source_session_id != session => repin(
                    prefix_digest,
                    boundaries.get(&(*source_session_id, *source_sequence)),
                )?,
                Event::ContextCompacted { record, .. } => repin(
                    &mut record.source_digest,
                    boundaries.get(&(session, record.covered_sequence)),
                )?,
                Event::GoalContextCompacted { record, .. } => repin(
                    &mut record.source_digest,
                    boundaries.get(&(session, record.covered_sequence)),
                )?,
                Event::WorkspaceRewindStarted { record } => {
                    repin(&mut record.prefix_digest, Some(&head))?;
                }
                _ => {}
            }
            let version = envelope.event.version();
            let payload = envelope.event.payload()?;
            if payload.len() > event_payload_limit(envelope.event.kind(), version) {
                return Err(StoreError::InvalidHistory);
            }
            let (run_id, agent_run_id) = (
                envelope.run_id.map(|id| id.to_string()),
                envelope.agent_run_id.map(|id| id.to_string()),
            );
            let stored = PrefixRow {
                sequence: envelope.sequence,
                session_id: &session.to_string(),
                run_id: run_id.as_deref(),
                agent_run_id: agent_run_id.as_deref(),
                kind: envelope.event.kind(),
                payload: &historical,
                created_at_ms: envelope.created_at_ms,
            };
            lane.historical.update_row(&stored)?;
            lane.upgraded.update(&envelope)?;
            let point = (session, envelope.sequence);
            if matches!(
                envelope.event,
                Event::RunFinished { .. }
                    | Event::SessionForked { .. }
                    | Event::WorkspaceRewindStarted { .. }
                    | Event::WorkspaceRewindFinished { .. }
                    | Event::ToolFinished { .. }
            ) {
                boundaries.insert(point, (lane.historical.digest(), lane.upgraded.digest()));
            }
            if (version, &payload) != (original.0, &original.1) {
                transaction.execute(
                    "UPDATE events SET event_version=?1, payload=?2 WHERE sequence=?3",
                    rusqlite::params![version, payload, sequence],
                )?;
                rewritten += 1;
            }
        }
    }
}

fn upgraded_row(registry: &Registry<'_>, mut row: RawRow) -> Result<RawRow, StoreError> {
    if let Some(payload) = registry.upgrade_payload(&row.kind, row.version, &row.payload)? {
        row.version = payload_version(&row.kind).ok_or(StoreError::InvalidHistory)?;
        row.payload_bytes = i64::try_from(payload.len()).map_err(|_| StoreError::InvalidHistory)?;
        row.payload = payload;
    }
    Ok(row)
}

fn validate_all(connection: &Connection, limit: usize) -> Result<(), StoreError> {
    validate_history(connection)?;
    // Inside a read transaction Session loads use the indexed per-Session query.
    let snapshot = connection.unchecked_transaction()?;
    let sessions = snapshot
        .prepare("SELECT DISTINCT session_id FROM events")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut loaded = 0_usize;
    for session in sessions {
        let id = SessionId::from_str(&session).map_err(|_| StoreError::InvalidHistory)?;
        let mut lineage = 0;
        resolve_view(&snapshot, id, 0, &mut lineage)?;
        loaded = loaded.saturating_add(lineage);
        if loaded > limit {
            return Err(StoreError::ReplayLimit);
        }
    }
    Ok(())
}

// True only when a read-only look proves the history unsupported, leaving its database and WAL as found.
pub(super) fn unsupported(root: &StateRoot) -> bool {
    matches!(
        inspect(&root.path().join(DATABASE_FILE)),
        Err(StoreError::InvalidHistory)
    ) && identity_of(&root.dir, DATABASE_FILE)
        .is_ok_and(|file| file.is_some_and(|(_, _, size)| size > 0))
}

fn inspect(path: &std::path::Path) -> Result<(), StoreError> {
    let connection = open_source(path, true)?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(StoreError::InvalidHistory);
    }
    validate_history(&connection)
}

fn migratable(connection: &Connection, registry: &Registry<'_>) -> Result<Option<i64>, StoreError> {
    let schema: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    // Every schema from the floor keeps `events (kind, event_version)`.
    let has_events: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name='events'",
        [],
        |row| row.get(0),
    )?;
    if !(registry.schema_floor..=SCHEMA_VERSION).contains(&schema)
        || has_events != 1
        || !exact_schema(connection, registry, schema)?
    {
        return Ok(None);
    }
    let mut needed = schema != SCHEMA_VERSION;
    let mut statement = connection.prepare("SELECT DISTINCT kind, event_version FROM events")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let (kind, version): (String, i64) = (row.get(0)?, row.get(1)?);
        match (payload_version(&kind), registry.event_floor(&kind)) {
            (Some(current), _) if version == current => {}
            (Some(current), Some(floor)) if (floor..current).contains(&version) => needed = true,
            _ => return Ok(None),
        }
    }
    Ok(needed.then_some(schema))
}

pub(super) fn schema_objects(connection: &Connection) -> Result<Vec<[String; 4]>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT type, name, tbl_name, COALESCE(sql, '') FROM sqlite_schema ORDER BY type, name",
    )?;
    let objects = statement
        .query_map([], |row| {
            let sql: String = row.get(3)?;
            Ok([
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                sql.split_whitespace().collect::<Vec<_>>().join(" "),
            ])
        })?
        .collect::<Result<_, _>>()?;
    Ok(objects)
}

// Compared before any mutation, so no trigger or view can run during the rewrite.
fn exact_schema(
    connection: &Connection,
    registry: &Registry<'_>,
    version: i64,
) -> Result<bool, StoreError> {
    let Some(sql) = registry.expected_schema(version) else {
        return Ok(false);
    };
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(sql)?;
    Ok(schema_objects(connection)? == schema_objects(&expected)?)
}

fn open_source(path: &std::path::Path, read_only: bool) -> Result<Connection, StoreError> {
    let access = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let connection = Connection::open_with_flags(path, access | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.execute_batch("PRAGMA trusted_schema=OFF;")?;
    if !read_only {
        // Writable opens finish SQLite recovery and fold any WAL into the main file.
        let mode: String =
            connection.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
        if mode != "delete" {
            return Err(StoreError::InvalidStateDirectory);
        }
    }
    Ok(connection)
}

fn recorded(path: &std::path::Path, id: &str) -> Result<bool, StoreError> {
    let connection = open_database(path, true)?;
    let count: i64 =
        connection.query_row("SELECT COUNT(*) FROM migrations WHERE id=?1", [id], |row| {
            row.get(0)
        })?;
    Ok(count == 1)
}

fn read_marker(root: &StateRoot) -> Result<Option<(Dir, Marker)>, StoreError> {
    let dir = match root.dir.open_dir_nofollow(DIR) {
        Ok(dir) => dir,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    crate::private_fs::check_directory(&dir, true)?;
    let Some(file) = open_private(&dir, MARKER, MAX_MARKER_BYTES)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(MAX_MARKER_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let marker: Marker =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::MigrationConflict)?;
    let backup = marker
        .backup
        .strip_suffix(".sqlite3")
        .and_then(|name| name.split_at_checked(28));
    if uuid::Uuid::parse_str(&marker.id).is_err()
        || !backup.is_some_and(|(stamp, id)| {
            valid_archive_name(stamp) && id.strip_prefix('-') == Some(marker.id.as_str())
        })
    {
        return Err(StoreError::MigrationConflict);
    }
    Ok(Some((dir, marker)))
}

fn write_marker(dir: &Dir, marker: &Marker) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec(marker).map_err(|_| StoreError::MigrationConflict)?;
    publish(dir, MARKER_TEMP, MARKER, &bytes)
}

fn open_private(
    dir: &Dir,
    name: &str,
    limit: usize,
) -> Result<Option<cap_std::fs::File>, StoreError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    nonblocking(&mut options);
    let file = match dir.open_with(name, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    check_private_record_file(&file, limit)?;
    Ok(Some(file))
}

fn create_private(dir: &Dir, name: &str) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    cap_std::fs::OpenOptionsExt::mode(&mut options, 0o600);
    nonblocking(&mut options);
    check_private_record_file(&dir.open_with(name, &options)?, 0)
}

fn sync_file(dir: &Dir, name: &str) -> Result<(), StoreError> {
    let file =
        open_private(dir, name, MAX_DATABASE_BYTES as usize)?.ok_or(StoreError::HistoryChanged)?;
    step()?;
    Ok(file.sync_all()?)
}

fn digest(dir: &Dir, name: &str) -> Result<Option<[u8; 32]>, StoreError> {
    let Some(mut file) = open_private(dir, name, MAX_DATABASE_BYTES as usize)? else {
        return Ok(None);
    };
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match file.read(&mut buffer)? {
            0 => return Ok(Some(hasher.finalize().into())),
            read => hasher.update(&buffer[..read]),
        }
    }
}

fn remove_if_present(dir: &Dir, name: &str) -> Result<(), StoreError> {
    step()?;
    match dir.remove_file(name) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn reserve_space(dir: &Dir, source_bytes: u64) -> Result<(), StoreError> {
    let stats = rustix::fs::fstatvfs(dir).map_err(io::Error::from)?;
    let available = stats.f_bavail.saturating_mul(stats.f_frsize);
    let needed = source_bytes
        .saturating_mul(2)
        .saturating_add(STAGING_HEADROOM_BYTES);
    if available < needed {
        return Err(StoreError::StorageFull);
    }
    Ok(())
}

#[cfg(not(test))]
fn now() -> SystemTime {
    SystemTime::now()
}

#[cfg(not(test))]
fn validation_limit() -> usize {
    MAX_VALIDATION_EVENTS
}

#[cfg(test)]
thread_local! {
    static CLOCK: std::cell::Cell<Option<SystemTime>> = const { std::cell::Cell::new(None) };
    static VALIDATION_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_VALIDATION_EVENTS) };
}

#[cfg(test)]
fn validation_limit() -> usize {
    VALIDATION_LIMIT.with(std::cell::Cell::get)
}

#[cfg(test)]
fn now() -> SystemTime {
    CLOCK
        .with(|clock| clock.get())
        .unwrap_or_else(SystemTime::now)
}

fn storage(error: StoreError) -> StoreError {
    match &error {
        StoreError::Io(source) if source.kind() == io::ErrorKind::StorageFull => {
            StoreError::StorageFull
        }
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::DiskFull =>
        {
            StoreError::StorageFull
        }
        _ => error,
    }
}
