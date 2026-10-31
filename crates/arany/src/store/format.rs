use super::journal::SCHEMA_VERSION;
use super::replay::validate_history;
use super::state::checkpoints;
use super::state::identity_of;
use super::{DATABASE_FILE, StateRoot, StoreError};
use rusqlite::{Connection, OpenFlags, config::DbConfig};

mod record;
#[cfg(test)]
mod tests;
pub use record::{RECORD_FORMATS, RecordFormat};

pub(super) enum Inspection {
    Current,
    Unsupported,
    Inconclusive,
}

pub(super) fn orphan_checkpoint_journal(root: &StateRoot) -> Result<bool, StoreError> {
    let records = checkpoints::records(root)?;
    let empty = records[0].as_ref().is_none_or(|file| file.len() == 0);
    Ok(empty && checkpoint_journal_present(root)?)
}

pub(super) fn checkpoint_journal_present(root: &StateRoot) -> Result<bool, StoreError> {
    Ok(identity_of(&root.dir, checkpoints::FILES[1])?.is_some())
}

// The caller holds the checkpoint lock; recovery also needs the exclusive history lock.
pub(super) fn checkpoint_incompatible(root: &StateRoot) -> Result<bool, StoreError> {
    let records = checkpoints::records(root)?;
    if records[0].as_ref().is_none_or(|file| file.len() == 0) {
        return Ok(false);
    }
    let path = root.path().join(checkpoints::FILES[0]);
    let unreadable = |error: &StoreError| {
        matches!(
            error,
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
                )
        )
    };
    if checkpoint_journal_present(root)? {
        match open_source(&path, false) {
            Ok(connection) => drop(connection),
            Err(error) if unreadable(&error) => {}
            Err(error) => return Err(error),
        }
        if checkpoint_journal_present(root)? {
            return Err(StoreError::InvalidStateDirectory);
        }
    }
    let inspect = || -> Result<i64, StoreError> {
        let connection = open_source(&path, true)?;
        connection.busy_timeout(std::time::Duration::ZERO)?;
        Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
    };
    match inspect() {
        Ok(version) => Ok(version != checkpoints::SCHEMA_VERSION),
        Err(error) if unreadable(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

// A read-only look leaves the database and WAL as found; only an inconclusive one needs a writable check.
pub(super) fn inspect_history(root: &StateRoot) -> Inspection {
    let nonempty = identity_of(&root.dir, DATABASE_FILE)
        .is_ok_and(|file| file.is_some_and(|(_, _, size)| size > 0));
    match inspect(&root.path().join(DATABASE_FILE)) {
        Ok(()) if nonempty => Inspection::Current,
        Err(StoreError::InvalidHistory) if nonempty => Inspection::Unsupported,
        _ => Inspection::Inconclusive,
    }
}

fn inspect(path: &std::path::Path) -> Result<(), StoreError> {
    let connection = open_source(path, true)?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(StoreError::InvalidHistory);
    }
    validate_history(&connection)
}

fn schema_objects(connection: &Connection) -> Result<Vec<[String; 4]>, StoreError> {
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

// Any extra trigger, view, table or index refuses, so none can run against a mismatched schema.
pub(super) fn exact_schema(connection: &Connection, sql: &str) -> Result<bool, StoreError> {
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(sql)?;
    let expected = schema_objects(&expected)?;
    let (count, oversized): (i64, i64) = connection.query_row(
        "SELECT COUNT(*), COUNT(CASE WHEN length(sql)>4096 THEN 1 END) FROM sqlite_schema",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count != expected.len() as i64 || oversized != 0 {
        return Ok(false);
    }
    Ok(schema_objects(connection)? == expected)
}

pub(super) fn open_source(
    path: &std::path::Path,
    read_only: bool,
) -> Result<Connection, StoreError> {
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
