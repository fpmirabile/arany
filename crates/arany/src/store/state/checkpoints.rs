use super::{StateRoot, StoreError, check_private_record_file, nonblocking};
use crate::session::{Checkpoint, CheckpointReason, CheckpointStatus, GoalId, RunId, SessionId};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{File, Metadata, MetadataExt, OpenOptions, OpenOptionsExt};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, TransactionBehavior, config::DbConfig, params,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

const FILE: &str = "checkpoints.sqlite";
const FILES: [&str; 4] = [
    FILE,
    "checkpoints.sqlite-journal",
    "checkpoints.sqlite-wal",
    "checkpoints.sqlite-shm",
];
const BUSY: Duration = Duration::from_secs(2);
const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const MAX_VERSION: usize = 1024 * 1024;
const MAX_CAPTURE: usize = 32 * 1024 * 1024;
const MAX_FILES: usize = 2048;
const HEADROOM: u64 = 4 * 1024 * 1024;

pub(crate) type Pruned = BTreeMap<String, String>;
pub(crate) type Checked = Option<i64>;

const SCHEMA: &[(&str, &str)] = &[
    (
        "blobs",
        "CREATE TABLE blobs(digest BLOB PRIMARY KEY CHECK(length(digest)=32), bytes BLOB NOT NULL, len INTEGER NOT NULL CHECK(len BETWEEN 0 AND 1048576 AND length(bytes)=len)) STRICT",
    ),
    (
        "refs",
        "CREATE TABLE refs(session_id TEXT NOT NULL, goal_id TEXT NOT NULL, run_id TEXT, rewind_id TEXT, tool_sequence INTEGER NOT NULL CHECK(tool_sequence>0), path TEXT NOT NULL, digest BLOB NOT NULL CHECK(length(digest)=32), len INTEGER NOT NULL CHECK(len BETWEEN 0 AND 1048576), captured_at_ms INTEGER NOT NULL, CHECK((run_id IS NULL) != (rewind_id IS NULL)), PRIMARY KEY(session_id,tool_sequence,path)) STRICT",
    ),
    (
        "refs_path",
        "CREATE INDEX refs_path ON refs(session_id,path,captured_at_ms,tool_sequence)",
    ),
    (
        "refs_goal",
        "CREATE INDEX refs_goal ON refs(goal_id,captured_at_ms,tool_sequence)",
    ),
    (
        "refs_session",
        "CREATE INDEX refs_session ON refs(session_id,captured_at_ms,tool_sequence)",
    ),
    ("refs_digest", "CREATE INDEX refs_digest ON refs(digest)"),
];

#[derive(Clone, Copy)]
struct Limits {
    path_versions: i64,
    path_bytes: i64,
    goal_bytes: i64,
    session_checkpoints: i64,
    session_bytes: i64,
    bytes: i64,
    refs: i64,
}
const LIMITS: Limits = Limits {
    path_versions: 32,
    path_bytes: 32 * 1024 * 1024,
    goal_bytes: 256 * 1024 * 1024,
    session_checkpoints: 100,
    session_bytes: 512 * 1024 * 1024,
    bytes: 2 * 1024 * 1024 * 1024,
    refs: 262_144,
};

pub(crate) struct Capture {
    pub(crate) session: SessionId,
    pub(crate) goal: GoalId,
    pub(crate) run: RunId,
    pub(crate) intent_id: uuid::Uuid,
    pub(crate) files: Vec<(String, Vec<u8>)>,
}

struct Lock {
    file: File,
    deadline: Instant,
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
    }
}

pub struct CheckpointReset {
    root: StateRoot,
    _lock: Lock,
    records: Vec<Option<Metadata>>,
}
impl CheckpointReset {
    pub fn apply(self) -> Result<(), StoreError> {
        self.root.readmit()?;
        let current = records(&self.root)?;
        if !self
            .records
            .iter()
            .zip(&current)
            .all(|(a, b)| match (a, b) {
                (None, None) => true,
                (Some(a), Some(b)) => {
                    a.dev() == b.dev()
                        && a.ino() == b.ino()
                        && a.len() == b.len()
                        && a.modified().ok() == b.modified().ok()
                        && a.ctime() == b.ctime()
                        && a.ctime_nsec() == b.ctime_nsec()
                }
                _ => false,
            })
        {
            return Err(StoreError::CheckpointChanged);
        }
        for (name, record) in FILES.iter().zip(&current) {
            if record.is_some() {
                self.root.dir.remove_file(name)?;
            }
        }
        self.root.dir.open(".")?.sync_all()?;
        Ok(())
    }
}

fn private_file(
    root: &StateRoot,
    name: &str,
    create: bool,
    limit: usize,
) -> Result<Option<File>, StoreError> {
    let mut options = OpenOptions::new();
    options.read(true).write(create).follow(FollowSymlinks::No);
    nonblocking(&mut options);
    let file = match root.dir.open_with(name, &options) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
            options.create_new(true).mode(0o600);
            root.dir.open_with(name, &options)?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    check_private_record_file(&file, limit)?;
    Ok(Some(file))
}

fn lock(root: &StateRoot) -> Result<Lock, StoreError> {
    root.readmit()?;
    let file = private_file(root, "checkpoints.lock", true, 0)?
        .ok_or(StoreError::InvalidStateDirectory)?;
    let deadline = Instant::now() + BUSY;
    loop {
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => break,
            Err(rustix::io::Errno::WOULDBLOCK) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(rustix::io::Errno::WOULDBLOCK) => return Err(StoreError::CheckpointUnavailable),
            Err(e) => return Err(std::io::Error::from(e).into()),
        }
    }
    let guard = Lock { file, deadline };
    let held = guard.file.metadata()?;
    let current = root.dir.symlink_metadata("checkpoints.lock")?;
    if held.dev() != current.dev() || held.ino() != current.ino() {
        return Err(StoreError::CheckpointChanged);
    }
    Ok(guard)
}

fn records(root: &StateRoot) -> Result<Vec<Option<Metadata>>, StoreError> {
    FILES
        .iter()
        .map(|name| {
            private_file(root, name, false, LIMITS.bytes as usize).and_then(|file| {
                file.map(|f| f.metadata().map_err(StoreError::from))
                    .transpose()
            })
        })
        .collect()
}

pub(crate) fn prepare_reset(root: &StateRoot) -> Result<CheckpointReset, StoreError> {
    let lock = lock(root)?;
    let records = records(root)?;
    // Healthy storage has no reset path; callers must first observe an unavailable store.
    match open_before(root, false, LIMITS, lock.deadline, &mut None) {
        Ok(_) => return Err(StoreError::CheckpointUnavailable),
        Err(StoreError::CheckpointUnavailable) => {}
        Err(StoreError::Sqlite(error)) => {
            if matches!(&error, rusqlite::Error::SqliteFailure(code, _) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
            {
                return Err(StoreError::Sqlite(error));
            }
        }
        Err(error) => return Err(error),
    }
    Ok(CheckpointReset {
        root: root.try_clone()?,
        _lock: lock,
        records,
    })
}

#[cfg(test)]
fn open(root: &StateRoot, create: bool, limits: Limits) -> Result<Connection, StoreError> {
    open_before(root, create, limits, Instant::now() + BUSY, &mut None)
}

fn open_before(
    root: &StateRoot,
    create: bool,
    limits: Limits,
    deadline: Instant,
    checked: &mut Checked,
) -> Result<Connection, StoreError> {
    records(root)?;
    let held = private_file(root, FILE, create, LIMITS.bytes as usize)?
        .ok_or(StoreError::CheckpointUnavailable)?;
    let flags = if create {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = Connection::open_with_flags(root.path().join(FILE), flags)?;
    for (limit, bytes) in [
        (
            rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
            (MAX_VERSION + 4096) as i32,
        ),
        (rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024),
    ] {
        connection.set_limit(limit, bytes)?;
        if connection.limit(limit)? != bytes {
            return Err(StoreError::CheckpointUnavailable);
        }
    }
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    if !connection.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE)? {
        return Err(StoreError::CheckpointUnavailable);
    }
    connection.execute_batch("PRAGMA trusted_schema=OFF;")?;
    connection.busy_timeout(deadline.saturating_duration_since(Instant::now()))?;
    let version: i64 = scalar(&connection, "PRAGMA user_version", [])?;
    let fresh = version == 0 && held.metadata()?.len() == 0 && create;
    if version != 1 && !fresh {
        return Err(StoreError::CheckpointUnavailable);
    }
    let schema = scalar(&connection, "PRAGMA schema_version", [])?;
    if !fresh && *checked != Some(schema) {
        validate(&connection, limits)?;
    }
    if create {
        connection.execute_batch(
            "PRAGMA page_size=4096; PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA;",
        )?;
        if fresh {
            connection.execute_batch("PRAGMA auto_vacuum=INCREMENTAL; BEGIN IMMEDIATE;")?;
            for (_, sql) in SCHEMA {
                connection.execute_batch(sql)?;
            }
            connection.execute_batch("PRAGMA user_version=1; COMMIT;")?;
        }
        let pages = limits.bytes / 4096;
        if scalar(&connection, &format!("PRAGMA max_page_count={pages}"), [])? != pages
            || scalar(&connection, "PRAGMA synchronous", [])? != 3
        {
            return Err(StoreError::CheckpointUnavailable);
        }
    }
    *checked = Some(scalar(&connection, "PRAGMA schema_version", [])?);
    let current = root.dir.symlink_metadata(FILE)?;
    let held = held.metadata()?;
    if current.dev() != held.dev() || current.ino() != held.ino() {
        return Err(StoreError::CheckpointChanged);
    }
    Ok(connection)
}

fn scalar(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<i64, StoreError> {
    Ok(connection.query_row(sql, params, |row| row.get(0))?)
}

fn validate(connection: &Connection, limits: Limits) -> Result<(), StoreError> {
    for (name, sql) in SCHEMA {
        let stored: String = connection.query_row(
            "SELECT sql FROM sqlite_schema WHERE name=?1 AND length(sql)<=4096",
            [name],
            |row| row.get(0),
        )?;
        if &stored != sql {
            return Err(StoreError::CheckpointUnavailable);
        }
    }
    if scalar(
        connection,
        "SELECT COUNT(*) FROM sqlite_schema WHERE sql IS NOT NULL",
        [],
    )? != SCHEMA.len() as i64
    {
        return Err(StoreError::CheckpointUnavailable);
    }
    let integrity: String = connection.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    let journal: String = connection.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    if integrity != "ok"
        || journal != "delete"
        || scalar(connection, "PRAGMA auto_vacuum", [])? != 2
        || scalar(connection, "PRAGMA page_size", [])? != 4096
        || scalar(connection, "PRAGMA page_count", [])? > limits.bytes / 4096
        || scalar(
            connection,
            "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
        )? != 2
        || scalar(connection, "SELECT COUNT(*) FROM refs", [])? > limits.refs
        || scalar(
            connection,
            "SELECT COUNT(*) FROM refs WHERE length(path)>4096 OR len<0 OR len>1048576 OR length(digest)!=32 OR (run_id IS NULL)=(rewind_id IS NULL)",
            [],
        )? != 0
        || scalar(
            connection,
            "SELECT COUNT(*) FROM blobs WHERE len<0 OR len>1048576 OR length(bytes)!=len OR length(digest)!=32",
            [],
        )? != 0
    {
        return Err(StoreError::CheckpointUnavailable);
    }
    Ok(())
}

fn collect_blobs(connection: &Connection) -> Result<(), StoreError> {
    connection.execute(
        "DELETE FROM blobs WHERE NOT EXISTS (SELECT 1 FROM refs WHERE refs.digest=blobs.digest)",
        [],
    )?;
    Ok(())
}

struct Victim {
    rowid: i64,
    session: String,
    run: Option<String>,
    digest: [u8; 32],
}

// Rows arrive ordered by eviction priority as (rowid, session, run, digest, len, group); whole groups
// are removed until `enough(freed_bytes, freed_groups)` holds, in one pass without rescanning refs.
fn evict(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    incoming: &BTreeSet<[u8; 32]>,
    pruned: &mut Pruned,
    must_satisfy: bool,
    mut enough: impl FnMut(i64, i64) -> bool,
) -> Result<(), StoreError> {
    let mut victims = Vec::new();
    let (mut bytes, mut groups) = (0i64, 0i64);
    let mut group = None::<String>;
    let mut satisfied = false;
    {
        let mut statement = connection.prepare(sql)?;
        let mut rows = statement.query(params)?;
        while let Some(row) = rows.next()? {
            let current: String = row.get(5)?;
            if group.as_deref() != Some(current.as_str()) {
                if !victims.is_empty() && enough(bytes, groups) {
                    satisfied = true;
                    break;
                }
                group = Some(current);
                groups += 1;
            }
            let digest: Vec<u8> = row.get(3)?;
            bytes += row.get::<_, i64>(4)?;
            victims.push(Victim {
                rowid: row.get(0)?,
                session: row.get(1)?,
                run: row.get(2)?,
                digest: digest
                    .try_into()
                    .map_err(|_| StoreError::CheckpointUnavailable)?,
            });
        }
    }
    if !satisfied && !victims.is_empty() && enough(bytes, groups) {
        satisfied = true;
    }
    if must_satisfy && !satisfied {
        return Err(StoreError::StorageFull);
    }
    let mut delete = connection.prepare("DELETE FROM refs WHERE rowid=?1")?;
    for victim in &victims {
        delete.execute([victim.rowid])?;
    }
    let mut lost = std::collections::HashMap::<[u8; 32], bool>::new();
    for victim in &victims {
        let lost = *match lost.entry(victim.digest) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let gone = !incoming.contains(&victim.digest)
                    && scalar(
                        connection,
                        "SELECT EXISTS(SELECT 1 FROM refs WHERE digest=?1)",
                        [victim.digest.as_slice()],
                    )? == 0;
                if gone {
                    connection.execute(
                        "DELETE FROM blobs WHERE digest=?1",
                        [victim.digest.as_slice()],
                    )?;
                }
                entry.insert(gone)
            }
        };
        if lost && let Some(run) = &victim.run {
            let newest = pruned.get(&victim.session).is_none_or(|known| known < run);
            if newest {
                pruned.insert(victim.session.clone(), run.clone());
            }
        }
    }
    Ok(())
}

const ROWS: &str = "SELECT rowid,session_id,run_id,digest,len";
const TOTAL_ORDER: &str = "SELECT rowid,session_id,run_id,digest,len,CAST(rowid AS TEXT) FROM (SELECT rowid,session_id,run_id,digest,len,captured_at_ms,tool_sequence,MAX(captured_at_ms) OVER (PARTITION BY session_id) m FROM refs) ORDER BY m,session_id,captured_at_ms,tool_sequence,rowid";

fn evict_total(
    connection: &Connection,
    incoming: &BTreeSet<[u8; 32]>,
    pruned: &mut Pruned,
    enough: impl FnMut(i64, i64) -> bool,
) -> Result<(), StoreError> {
    evict(connection, TOTAL_ORDER, [], incoming, pruned, true, enough)
}

fn evict_checkpoints(
    connection: &Connection,
    filter: &str,
    owner: &str,
    run: &str,
    incoming: &BTreeSet<[u8; 32]>,
    pruned: &mut Pruned,
    enough: impl FnMut(i64, i64) -> bool,
) -> Result<(), StoreError> {
    let sql = format!(
        "SELECT rowid,session_id,run_id,digest,len,session_id||'/'||c FROM (SELECT rowid,session_id,run_id,digest,len,COALESCE(run_id,rewind_id) c,MIN(captured_at_ms) OVER w t,MIN(tool_sequence) OVER w q FROM refs WHERE {filter} AND COALESCE(run_id,rewind_id)!=?2 WINDOW w AS (PARTITION BY session_id,COALESCE(run_id,rewind_id))) ORDER BY t,q,session_id,c,rowid"
    );
    evict(
        connection,
        &sql,
        params![owner, run],
        incoming,
        pruned,
        true,
        enough,
    )
}

fn expire(
    connection: &Connection,
    now: i64,
    incoming: &BTreeSet<[u8; 32]>,
    pruned: &mut Pruned,
) -> Result<(), StoreError> {
    evict(
        connection,
        &format!(
            "{ROWS},CAST(rowid AS TEXT) FROM refs WHERE session_id IN (SELECT session_id FROM refs GROUP BY session_id HAVING MAX(captured_at_ms)<?1 OR MAX(captured_at_ms)>?2)"
        ),
        params![now.saturating_sub(RETENTION_MS), now],
        incoming,
        pruned,
        false,
        |_, _| false,
    )
}

pub(crate) fn sweep(
    root: &StateRoot,
    now: i64,
    checked: &mut Checked,
) -> Result<Pruned, StoreError> {
    let _lock = lock(root)?;
    let mut connection = open_before(root, true, LIMITS, _lock.deadline, checked)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut pruned = Pruned::new();
    expire(&tx, now, &BTreeSet::new(), &mut pruned)?;
    collect_blobs(&tx)?;
    tx.commit()?;
    connection.execute_batch("PRAGMA incremental_vacuum;")?;
    Ok(pruned)
}

pub(crate) fn capture(
    root: &StateRoot,
    capture: Capture,
    sequence: u64,
    now: i64,
    checked: &mut Checked,
) -> Checkpoint {
    if capture.files.len() > MAX_FILES
        || capture
            .files
            .iter()
            .try_fold(0usize, |total, (_, bytes)| total.checked_add(bytes.len()))
            .is_none_or(|bytes| bytes > MAX_CAPTURE)
        || capture.files.iter().any(|(_, b)| b.len() > MAX_VERSION)
    {
        return Checkpoint::skipped(CheckpointReason::TooLarge);
    }
    match capture_inner(root, &capture, sequence, now, LIMITS, checked) {
        Ok(pruned) => Checkpoint {
            status: CheckpointStatus::Captured,
            reason: None,
            pruned_before_run: pruned
                .get(&capture.session.to_string())
                .and_then(|run| run.parse().ok()),
        },
        Err(StoreError::StorageFull) => Checkpoint::skipped(CheckpointReason::Quota),
        Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)))
            if error.code == rusqlite::ErrorCode::DiskFull =>
        {
            Checkpoint::skipped(CheckpointReason::Quota)
        }
        Err(_) => Checkpoint::skipped(CheckpointReason::StoreUnavailable),
    }
}

fn in_time(deadline: Instant) -> Result<(), StoreError> {
    if Instant::now() > deadline {
        return Err(StoreError::CheckpointUnavailable);
    }
    Ok(())
}

fn capture_inner(
    root: &StateRoot,
    capture: &Capture,
    sequence: u64,
    now: i64,
    limits: Limits,
    checked: &mut Checked,
) -> Result<Pruned, StoreError> {
    let _lock = lock(root)?;
    let deadline = _lock.deadline;
    let mut connection = open_before(root, true, limits, deadline, checked)?;
    let capacity = rustix::fs::fstatvfs(&root.dir).map_err(|e| StoreError::Io(e.into()))?;
    let available =
        i64::try_from(capacity.f_bavail.saturating_mul(capacity.f_frsize)).unwrap_or(i64::MAX);
    let bytes: i64 = capture.files.iter().map(|(_, b)| b.len() as i64).sum();
    let files = capture.files.len() as i64;
    let incoming: BTreeSet<[u8; 32]> = capture
        .files
        .iter()
        .map(|(_, bytes)| Sha256::digest(bytes).into())
        .collect();
    let mut pruned = Pruned::new();
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    expire(&tx, now, &incoming, &mut pruned)?;
    tx.commit()?;
    in_time(deadline)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let session = capture.session.to_string();
    let goal = capture.goal.to_string();
    let run = capture.run.to_string();
    let sequence = i64::try_from(sequence).map_err(|_| StoreError::CheckpointUnavailable)?;
    for (path, content) in &capture.files {
        if !crate::tools::types::valid_relative(path, false) || path.len() > 4096 {
            return Err(StoreError::CheckpointUnavailable);
        }
        let count = scalar(
            &tx,
            "SELECT COUNT(*) FROM refs WHERE session_id=?1 AND path=?2",
            params![session, path],
        )?;
        let len = scalar(
            &tx,
            "SELECT COALESCE(SUM(len),0) FROM refs WHERE session_id=?1 AND path=?2",
            params![session, path],
        )?;
        let add = content.len() as i64;
        if count + 1 > limits.path_versions || len + add > limits.path_bytes {
            evict(
                &tx,
                &format!(
                    "{ROWS},CAST(rowid AS TEXT) FROM refs WHERE session_id=?1 AND path=?2 AND tool_sequence!=?3 ORDER BY captured_at_ms,tool_sequence,rowid"
                ),
                params![session, path, sequence],
                &incoming,
                &mut pruned,
                true,
                |freed, rows| {
                    count + 1 - rows <= limits.path_versions
                        && len - freed + add <= limits.path_bytes
                },
            )?;
        }
    }
    in_time(deadline)?;
    let goal_bytes = scalar(
        &tx,
        "SELECT COALESCE(SUM(len),0) FROM refs WHERE goal_id=?1",
        [&goal],
    )?;
    if goal_bytes + bytes > limits.goal_bytes {
        evict_checkpoints(
            &tx,
            "goal_id=?1",
            &goal,
            &run,
            &incoming,
            &mut pruned,
            |freed, _| goal_bytes - freed + bytes <= limits.goal_bytes,
        )?;
    }
    let session_bytes = scalar(
        &tx,
        "SELECT COALESCE(SUM(len),0) FROM refs WHERE session_id=?1",
        [&session],
    )?;
    let session_checkpoints = scalar(
        &tx,
        "SELECT COUNT(DISTINCT COALESCE(run_id,rewind_id)) FROM refs WHERE session_id=?1",
        [&session],
    )? + i64::from(
        scalar(
            &tx,
            "SELECT COUNT(*) FROM refs WHERE session_id=?1 AND run_id=?2",
            params![session, run],
        )? == 0,
    );
    if session_bytes + bytes > limits.session_bytes
        || session_checkpoints > limits.session_checkpoints
    {
        evict_checkpoints(
            &tx,
            "session_id=?1",
            &session,
            &run,
            &incoming,
            &mut pruned,
            |freed, groups| {
                session_bytes - freed + bytes <= limits.session_bytes
                    && session_checkpoints - groups <= limits.session_checkpoints
            },
        )?;
    }
    in_time(deadline)?;
    let rows = scalar(&tx, "SELECT COUNT(*) FROM refs", [])?;
    if rows + files > limits.refs {
        evict_total(&tx, &incoming, &mut pruned, |_, removed| {
            rows - removed + files <= limits.refs
        })?;
    }
    in_time(deadline)?;
    let needed = bytes + files * 16 * 4096;
    let pages = |tx: &Connection| -> Result<(i64, i64), StoreError> {
        let free = scalar(tx, "PRAGMA freelist_count", [])? * 4096;
        Ok((scalar(tx, "PRAGMA page_count", [])? * 4096 - free, free))
    };
    let shortfall = |tx: &Connection| -> Result<i64, StoreError> {
        let (used, free) = pages(tx)?;
        let growth = (needed - free).max(0);
        Ok((used + needed - limits.bytes).max(HEADROOM as i64 + needed + growth - available))
    };
    let short = shortfall(&tx)?;
    if short > 0 {
        if HEADROOM as i64 + needed > available {
            return Err(StoreError::StorageFull);
        }
        evict_total(&tx, &incoming, &mut pruned, |freed, _| freed >= short)?;
        in_time(deadline)?;
        if shortfall(&tx)? > 0 {
            return Err(StoreError::StorageFull);
        }
    }
    for (path, content) in &capture.files {
        let digest = Sha256::digest(content);
        tx.execute(
            "INSERT INTO blobs(digest,bytes,len) VALUES(?1,?2,?3) ON CONFLICT(digest) DO NOTHING",
            params![digest.as_slice(), content, content.len() as i64],
        )?;
        let stored = read(&tx, digest.as_slice())?.ok_or(StoreError::CheckpointUnavailable)?;
        if &stored != content {
            return Err(StoreError::CheckpointUnavailable);
        }
        tx.execute("INSERT INTO refs(session_id,goal_id,run_id,rewind_id,tool_sequence,path,digest,len,captured_at_ms) VALUES(?1,?2,?3,NULL,?4,?5,?6,?7,?8)", params![session,goal,run,sequence,path,digest.as_slice(),content.len() as i64,now])?;
    }
    in_time(deadline)?;
    tx.commit()?;
    connection.execute_batch("PRAGMA incremental_vacuum;")?;
    Ok(pruned)
}

fn read(connection: &Connection, digest: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
    let bytes: Option<Vec<u8>> = connection.query_row("SELECT bytes FROM blobs WHERE digest=?1 AND len BETWEEN 0 AND 1048576 AND length(bytes)=len",[digest],|r|r.get(0)).optional()?;
    Ok(bytes.filter(|bytes| Sha256::digest(bytes).as_slice() == digest))
}

pub(crate) fn load(
    root: &StateRoot,
    digest: [u8; 32],
    checked: &mut Checked,
) -> Result<Option<Vec<u8>>, StoreError> {
    let _lock = lock(root)?;
    read(
        &open_before(root, false, LIMITS, _lock.deadline, checked)?,
        &digest,
    )
}

#[cfg(test)]
mod tests;
