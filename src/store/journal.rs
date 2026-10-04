use super::replay::{MAX_SESSION_EVENTS, resolve_open_view, resolve_view};
use super::{DATABASE_FILE, MAX_DATABASE_BYTES, StateRoot, StoreError, event_payload_limit};
use crate::session::{
    CompactionStatus, Event, EventEnvelope, RunStatus, SessionId, SessionView, prefix_digest,
    validate_compaction_references,
};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, config::DbConfig, params};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(all(test, target_os = "linux"))]
mod crash_tests;
#[cfg(test)]
mod tests;

const ADMISSION_HEADROOM_BYTES: i64 = 4 * 1024 * 1024;
const EVENT_SQLITE_OVERHEAD_PAGES: i64 = 16;
const MAX_HOT_SESSION_PAYLOAD_BYTES: usize = 128 * 1024 * 1024;

pub(super) struct HotSession {
    session_id: SessionId,
    view: SessionView,
    total_events: usize,
    payload_bytes: usize,
    data_version: i64,
    global_sequence: u64,
}

pub(super) fn admit_operation(
    connection: &mut Connection,
    session_id: SessionId,
    event_slots: usize,
) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut count = 0;
    resolve_open_view(&transaction, session_id, &mut count)?;
    if count
        .checked_add(event_slots)
        .is_none_or(|total| total > MAX_SESSION_EVENTS)
    {
        return Err(StoreError::ReplayLimit);
    }
    let page_count: i64 = transaction.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: i64 = transaction.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let event_bytes = i64::try_from(event_slots).map_err(|_| StoreError::ReplayLimit)?
        * (super::MAX_EVENT_BYTES as i64 + EVENT_SQLITE_OVERHEAD_PAGES * page_size);
    let image_message_extra =
        (event_payload_limit("MessageAccepted", 2) - super::MAX_EVENT_BYTES) as i64;
    if MAX_DATABASE_BYTES - page_count * page_size
        < ADMISSION_HEADROOM_BYTES + event_bytes + image_message_extra
    {
        return Err(StoreError::StorageFull);
    }
    transaction.commit()?;
    Ok(())
}

pub(super) fn open_connection(root: &StateRoot, read_only: bool) -> Result<Connection, StoreError> {
    let path = root.path().join(DATABASE_FILE);
    let access = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let flags = access | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = Connection::open_with_flags(path, flags)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    if !connection.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE)? {
        return Err(StoreError::InvalidStateDirectory);
    }
    connection.busy_timeout(std::time::Duration::from_millis(250))?;
    if read_only {
        connection.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")?;
    } else {
        connection.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA page_size=4096;")?;
    }
    let journal: String = if read_only {
        connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?
    } else {
        connection.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?
    };
    if !read_only {
        connection.execute_batch("PRAGMA synchronous=EXTRA;")?;
    }
    let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let trusted: i64 = connection.query_row("PRAGMA trusted_schema", [], |row| row.get(0))?;
    if journal != "delete" || page_size != 4096 || trusted != 0 {
        return Err(StoreError::InvalidStateDirectory);
    }
    if !read_only {
        let synchronous: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
        if synchronous != 3 {
            return Err(StoreError::InvalidStateDirectory);
        }
    }
    let max_pages = MAX_DATABASE_BYTES / page_size;
    let pages: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    if pages > max_pages {
        return Err(StoreError::StorageFull);
    }
    if !read_only {
        let applied_max: i64 =
            connection.query_row(&format!("PRAGMA max_page_count={max_pages}"), [], |row| {
                row.get(0)
            })?;
        if applied_max != max_pages {
            return Err(StoreError::InvalidStateDirectory);
        }
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match version {
        0 if !read_only => connection.execute_batch(
            "BEGIN IMMEDIATE;
            CREATE TABLE events (
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
            ) STRICT;
            PRAGMA user_version=3;
            COMMIT;",
        )?,
        1 if !read_only => connection.execute_batch(
            "BEGIN IMMEDIATE;
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
            ) STRICT;
            PRAGMA user_version=3;
            COMMIT;",
        )?,
        2 if !read_only => connection.execute_batch(
            "BEGIN IMMEDIATE;
            CREATE TABLE native_provider_evidence (
                fingerprint BLOB PRIMARY KEY CHECK(length(fingerprint) = 32),
                checked_at_ms INTEGER NOT NULL,
                expires_at_ms INTEGER NOT NULL
            ) STRICT;
            PRAGMA user_version=3;
            COMMIT;",
        )?,
        1..=3 => {}
        _ => return Err(StoreError::InvalidHistory),
    }
    Ok(connection)
}

pub(super) fn append(
    connection: &mut Connection,
    hot_session: &mut Option<HotSession>,
    session_id: SessionId,
    event: Event,
) -> Result<EventEnvelope, StoreError> {
    let cached = hot_session.take();
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (envelope, next_cache) = append_in_transaction(&transaction, cached, session_id, event)?;
    #[cfg(all(test, target_os = "linux"))]
    crash_tests::before_commit();
    transaction.commit()?;
    *hot_session = next_cache;
    Ok(envelope)
}

fn append_in_transaction(
    transaction: &Transaction<'_>,
    cached: Option<HotSession>,
    session_id: SessionId,
    event: Event,
) -> Result<(EventEnvelope, Option<HotSession>), StoreError> {
    let payload = event.payload()?;
    if payload.len() > event_payload_limit(event.kind(), event.version()) {
        return Err(StoreError::StorageFull);
    }
    let data_version: i64 = transaction.query_row("PRAGMA data_version", [], |row| row.get(0))?;
    let sequence: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events",
        [],
        |row| row.get(0),
    )?;
    let sequence = u64::try_from(sequence).map_err(|_| StoreError::InvalidHistory)?;
    let cached = cached.filter(|cache| {
        cache.session_id == session_id
            && cache.data_version == data_version
            && cache.global_sequence.checked_add(1) == Some(sequence)
            && !matches!(
                event,
                Event::ContextCompacted { .. } | Event::SessionForked { .. }
            )
    });
    let (existing, mut prior_view, total_events, cache_bytes) = if let Some(cache) = cached {
        (
            None,
            Some(cache.view),
            cache.total_events,
            Some(cache.payload_bytes),
        )
    } else {
        let mut total_events = 0;
        let (events, view) = resolve_open_view(transaction, session_id, &mut total_events)?;
        let bytes = if view.as_ref().is_none_or(|view| view.lineage.is_none()) {
            bounded_payload_bytes(&events)?
        } else {
            None
        };
        (Some(events), view, total_events, bytes)
    };
    if total_events == MAX_SESSION_EVENTS {
        return Err(StoreError::ReplayLimit);
    }
    let page_count: i64 = transaction.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: i64 = transaction.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    if MAX_DATABASE_BYTES - page_count * page_size < ADMISSION_HEADROOM_BYTES {
        return Err(StoreError::StorageFull);
    }
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::InvalidStateDirectory)?
        .as_millis();
    let envelope = EventEnvelope {
        sequence,
        session_id,
        run_id: event.scope().0,
        agent_run_id: event.scope().1,
        event,
        created_at_ms: i64::try_from(created_at_ms).map_err(|_| StoreError::InvalidHistory)?,
    };
    if let Event::MessageAccepted { run_id, .. } = &envelope.event
        && prior_view
            .as_ref()
            .is_some_and(|view| view.runs.iter().any(|run| run.id == *run_id))
    {
        return Err(StoreError::InvalidHistory);
    }
    if let Event::RunStarted { config, .. } = &envelope.event
        && let Some(selected_sequence) = config.compaction_event_sequence
    {
        let selected = prior_view
            .as_ref()
            .and_then(|view| {
                view.compactions
                    .iter()
                    .find(|value| value.event_sequence == selected_sequence)
            })
            .ok_or(StoreError::InvalidSnapshot)?;
        let CompactionStatus::Succeeded { content_digest, .. } = &selected.record.status else {
            return Err(StoreError::InvalidSnapshot);
        };
        if config.compaction_content_digest != Some(*content_digest)
            || selected.event_sequence >= envelope.sequence
            || selected.record.covered_sequence >= envelope.sequence
        {
            return Err(StoreError::InvalidSnapshot);
        }
    }
    if matches!(
        envelope.event,
        Event::ContextCompacted { .. } | Event::SessionForked { .. }
    ) {
        let mut candidate = existing.ok_or(StoreError::InvalidHistory)?;
        candidate.push(envelope.clone());
        prior_view = SessionView::replay_open(session_id, &candidate)?;
    } else if let Some(view) = &mut prior_view {
        view.apply_committed(&envelope)?;
        if matches!(envelope.event, Event::RunStarted { .. }) {
            validate_compaction_references(view)?;
        }
    } else {
        prior_view = SessionView::replay_open(session_id, std::slice::from_ref(&envelope))?;
    }
    transaction.execute(
        "INSERT INTO events (sequence, session_id, run_id, agent_run_id, kind, event_version, payload, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            i64::try_from(sequence).map_err(|_| StoreError::InvalidHistory)?,
            session_id.to_string(),
            envelope.run_id.map(|id| id.to_string()),
            envelope.agent_run_id.map(|id| id.to_string()),
            envelope.event.kind(),
            envelope.event.version(),
            payload,
            envelope.created_at_ms,
        ],
    )?;
    let next_cache = cache_bytes
        .and_then(|bytes| bytes.checked_add(payload.len()))
        .filter(|&bytes| bytes <= MAX_HOT_SESSION_PAYLOAD_BYTES)
        .and_then(|payload_bytes| {
            prior_view
                .filter(|view| view.lineage.is_none())
                .map(|view| HotSession {
                    session_id,
                    view,
                    total_events: total_events + 1,
                    payload_bytes,
                    data_version,
                    global_sequence: sequence,
                })
        });
    Ok((envelope, next_cache))
}

fn bounded_payload_bytes(events: &[EventEnvelope]) -> Result<Option<usize>, StoreError> {
    let mut bytes = 0_usize;
    for envelope in events {
        bytes = bytes
            .checked_add(envelope.event.payload()?.len())
            .ok_or(StoreError::StorageFull)?;
        if bytes > MAX_HOT_SESSION_PAYLOAD_BYTES {
            return Ok(None);
        }
    }
    Ok(Some(bytes))
}

pub(super) fn fork_session(
    connection: &mut Connection,
    source_session_id: SessionId,
    title: String,
) -> Result<SessionId, StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (events, view) = resolve_view(&transaction, source_session_id, 0, &mut 0)?;
    let view = view.ok_or(StoreError::InvalidHistory)?;
    let last_run = view.runs.last().ok_or(StoreError::InvalidHistory)?;
    if !matches!(
        last_run.status,
        RunStatus::Finished | RunStatus::Failed | RunStatus::Cancelled
    ) {
        return Err(StoreError::InvalidHistory);
    }
    let source_run_id = last_run.id;
    let source_sequence = events
        .iter()
        .rev()
        .find_map(|envelope| match &envelope.event {
            Event::RunFinished { run_id, .. } if *run_id == source_run_id => {
                Some(envelope.sequence)
            }
            Event::SessionForked {
                source_run_id: run_id,
                ..
            } if *run_id == source_run_id => Some(envelope.sequence),
            _ => None,
        })
        .ok_or(StoreError::InvalidHistory)?;
    let digest = prefix_digest(&events, source_sequence)?;
    let id = SessionId::new();
    append_in_transaction(
        &transaction,
        None,
        id,
        Event::SessionForked {
            title,
            source_session_id,
            source_run_id,
            source_sequence,
            prefix_digest: digest,
        },
    )?;
    transaction.commit()?;
    Ok(id)
}
