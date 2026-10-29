use super::{ResolvedSession, SessionPage, StoreError, event_payload_limit};
use crate::session::ForkInheritance;
use crate::session::{
    AgentRunId, Event, EventEnvelope, ForkLineage, RunConfig, RunId, SessionDefaults, SessionId,
    SessionListItem, SessionView, WorkspaceIdentity, prefix_digest, validate_compaction_references,
};
use rusqlite::{Connection, OptionalExtension, Row};
use std::collections::HashMap;
use std::str::FromStr;

pub(crate) const MAX_SESSION_EVENTS: usize = 10_000;
const MAX_EVENT_KIND_BYTES: usize = 32;
const INVALID_KIND_MARKER: &str = "[invalid kind]";

#[derive(Debug)]
pub struct InspectedEvent {
    pub sequence: u64,
    pub kind: String,
    pub event: Result<EventEnvelope, crate::session::ReplayError>,
}

#[derive(Debug)]
pub struct SessionInspection {
    pub events: Vec<InspectedEvent>,
    pub failure: Option<InspectionFailure>,
    pub prefix_view: Option<SessionView>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectionFailure {
    pub sequence: u64,
    pub cause: InspectionCause,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InspectionCause {
    Replay(crate::session::ReplayError),
    JournalSequenceGap,
}

impl std::fmt::Display for InspectionCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Replay(cause) => cause.fmt(formatter),
            Self::JournalSequenceGap => formatter.write_str("invalid journal sequence"),
        }
    }
}
const MAX_FORK_DEPTH: usize = 8;
pub(crate) const MAX_DISCOVERED_SESSIONS: usize = 65_536;
pub(crate) const MAX_LIST_SCAN_EVENTS: usize = 262_144;
const LIST_HEAD_CHUNK: usize = 1024;
const MAX_LIST_BUSY_RETRIES: usize = 3;
pub(super) const WATERMARK_SQL: &str = "SELECT COALESCE(MAX(sequence), 0) FROM events";
pub(super) const NEXT_HEAD_SQL: &str =
    "SELECT session_id FROM events WHERE session_id > ?1 ORDER BY session_id LIMIT 1";
pub(super) const HEAD_TAIL_SQL: &str = "SELECT sequence FROM events
     WHERE session_id = ?1 AND sequence <= ?2 ORDER BY sequence DESC LIMIT 1";
pub(super) const LINEAGE_COUNT_SQL: &str =
    "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND sequence <= ?2";
macro_rules! raw_select {
    ($tail:literal) => {
        concat!(
            "SELECT sequence,
                length(CAST(session_id AS BLOB)),
                length(CAST(kind AS BLOB)),
                length(CAST(payload AS BLOB)),
                session_id, run_id, agent_run_id, kind, event_version, payload, created_at_ms
         FROM events ",
            $tail
        )
    };
}
pub(super) const HEAD_FIRST_SQL: &str =
    raw_select!("WHERE session_id = ?1 AND sequence <= ?2 ORDER BY sequence ASC LIMIT 1");
pub(super) const LANE_SQL: &str = raw_select!(
    "WHERE session_id = ?1 AND sequence > ?2 AND sequence <= ?3 ORDER BY sequence ASC LIMIT 256"
);
const LIST_EVENT_CHUNK: usize = 256;
pub(super) const RUN_OWNER_SQL: &str =
    "SELECT DISTINCT session_id FROM events WHERE run_id=?1 LIMIT 2";
pub(super) const SESSION_EVENTS_SQL: &str = "SELECT sequence,
            length(CAST(session_id AS BLOB)),
            length(CAST(kind AS BLOB)),
            length(CAST(payload AS BLOB)),
            session_id, run_id, agent_run_id, kind, event_version, payload, created_at_ms
     FROM events WHERE session_id=?1 ORDER BY sequence LIMIT ?2";

struct SessionHead {
    title: String,
    direct_identity: Option<WorkspaceIdentity>,
    source_session_id: Option<SessionId>,
    source_sequence: Option<u64>,
    last_sequence: u64,
    created_at_ms: i64,
    last_activity_at_ms: i64,
    explicit_title: bool,
    fallback_title: Option<(u64, String)>,
    defaults: SessionDefaults,
    run_defaults: SessionDefaults,
}

pub(super) fn load_session(
    connection: &Connection,
    session_id: SessionId,
) -> Result<Vec<EventEnvelope>, StoreError> {
    resolve_view(connection, session_id, 0, &mut 0).map(|(events, _)| events)
}

pub(super) fn inspect_session(
    connection: &Connection,
    session_id: SessionId,
) -> Result<SessionInspection, StoreError> {
    let (events, mut failure) = scan_inspected_events(connection, session_id)?;
    let replay_limit = failure.as_ref().map_or(u64::MAX, |f| f.sequence);
    if let Some(replay_failure) = replay_failure(connection, session_id, &events, replay_limit)?
        && failure
            .as_ref()
            .is_none_or(|current| replay_failure.sequence < current.sequence)
    {
        failure = Some(replay_failure);
    }
    let prefix_view = project_inspected_prefix(connection, session_id, &events, &failure)?;
    Ok(SessionInspection {
        events,
        failure,
        prefix_view,
    })
}

fn scan_inspected_events(
    connection: &Connection,
    session_id: SessionId,
) -> Result<(Vec<InspectedEvent>, Option<InspectionFailure>), StoreError> {
    let mut statement = connection.prepare(&format!(
        "SELECT sequence, length(CAST(session_id AS BLOB)), length(CAST(kind AS BLOB)), length(CAST(payload AS BLOB)), session_id, run_id, agent_run_id, CASE WHEN length(CAST(kind AS BLOB)) BETWEEN 1 AND {MAX_EVENT_KIND_BYTES} THEN kind ELSE '{INVALID_KIND_MARKER}' END, event_version, payload, created_at_ms, CAST(session_id AS BLOB) = CAST(?1 AS BLOB) FROM events ORDER BY sequence LIMIT {}",
        MAX_LIST_SCAN_EVENTS + 1
    ))?;
    let mut rows = statement.query([session_id.to_string()])?;
    let mut failure = None;
    let mut events = Vec::new();
    let mut expected = 1_i64;
    let mut scanned = 0;
    while let Some(row) = rows.next()? {
        scanned += 1;
        if scanned > MAX_LIST_SCAN_EVENTS {
            return Err(StoreError::ReplayLimit);
        }
        let sequence: i64 = row.get(0)?;
        let sequence_for_report = u64::try_from(sequence).unwrap_or_default();
        if sequence != expected && failure.is_none() {
            failure = Some(InspectionFailure {
                sequence: sequence_for_report,
                cause: InspectionCause::JournalSequenceGap,
            });
        }
        expected = sequence.checked_add(1).ok_or(StoreError::InvalidHistory)?;
        let selected: bool = row.get(11)?;
        let kind = inspected_kind(row);
        let raw = match raw_row(row) {
            Ok(raw) => raw,
            Err(_) => {
                if failure.is_none() {
                    failure = Some(InspectionFailure {
                        sequence: sequence_for_report,
                        cause: InspectionCause::Replay(crate::session::ReplayError::InvalidPayload),
                    });
                }
                if selected {
                    push_inspected_event(
                        &mut events,
                        sequence_for_report,
                        kind,
                        Err(crate::session::ReplayError::InvalidPayload),
                    )?;
                }
                continue;
            }
        };
        let event = raw.decode_inspected();
        if let Err(cause) = &event
            && failure.is_none()
        {
            failure = Some(InspectionFailure {
                sequence: sequence_for_report,
                cause: InspectionCause::Replay(*cause),
            });
        }
        if selected {
            push_inspected_event(&mut events, sequence_for_report, kind, event)?;
        }
    }
    Ok((events, failure))
}

fn push_inspected_event(
    events: &mut Vec<InspectedEvent>,
    sequence: u64,
    kind: String,
    event: Result<EventEnvelope, crate::session::ReplayError>,
) -> Result<(), StoreError> {
    if events.len() == MAX_SESSION_EVENTS {
        return Err(StoreError::ReplayLimit);
    }
    events.push(InspectedEvent {
        sequence,
        kind,
        event,
    });
    Ok(())
}

fn inspected_kind(row: &Row<'_>) -> String {
    let bytes = match row.get_ref(7) {
        Ok(rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes)) => {
            bytes
        }
        _ => return INVALID_KIND_MARKER.into(),
    };
    if !(1..=MAX_EVENT_KIND_BYTES).contains(&bytes.len()) {
        return INVALID_KIND_MARKER.into();
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .unwrap_or_else(|_| INVALID_KIND_MARKER.into())
}

fn project_inspected_prefix(
    connection: &Connection,
    session_id: SessionId,
    events: &[InspectedEvent],
    failure: &Option<InspectionFailure>,
) -> Result<Option<SessionView>, StoreError> {
    let prefix: Vec<_> = events
        .iter()
        .take_while(|item| failure.as_ref().is_none_or(|f| item.sequence < f.sequence))
        .filter_map(|item| item.event.as_ref().ok().cloned())
        .collect();
    let mut total_events = prefix.len();
    match replay_through(
        connection,
        session_id,
        &prefix,
        0,
        &mut total_events,
        true,
        None,
    ) {
        Ok(view) => Ok(view),
        Err(error) => inspection_rejection(error).map(|_| None),
    }
}

fn replay_failure(
    connection: &Connection,
    session_id: SessionId,
    events: &[InspectedEvent],
    before_sequence: u64,
) -> Result<Option<InspectionFailure>, StoreError> {
    let decoded = decoded_prefix(events, before_sequence);
    let mut failures = compaction_failures(&decoded);
    if let Some(failure) = canonical_replay_failure(connection, session_id, &decoded)? {
        failures.push(failure);
    }
    Ok(failures.into_iter().min_by_key(|failure| failure.sequence))
}

fn decoded_prefix(events: &[InspectedEvent], before_sequence: u64) -> Vec<EventEnvelope> {
    events
        .iter()
        .take_while(|item| item.sequence < before_sequence)
        .filter_map(|item| item.event.as_ref().ok().cloned())
        .collect()
}

fn compaction_failures(events: &[EventEnvelope]) -> Vec<InspectionFailure> {
    let mut failures = Vec::new();
    for result in [
        crate::session::validate_compactions_with_sequence(events),
        crate::session::validate_goal_context_compactions_with_sequence(events),
    ] {
        if let Err((sequence, cause)) = result {
            failures.push(InspectionFailure {
                sequence,
                cause: InspectionCause::Replay(cause),
            });
        }
    }
    failures
}

fn canonical_replay_failure(
    connection: &Connection,
    session_id: SessionId,
    events: &[EventEnvelope],
) -> Result<Option<InspectionFailure>, StoreError> {
    if events.is_empty() {
        return Ok(None);
    }
    let mut total_events = events.len();
    let replayed = replay_through(
        connection,
        session_id,
        events,
        0,
        &mut total_events,
        true,
        None,
    );
    let error = match replayed {
        Ok(_) => return Ok(None),
        Err(error) => inspection_rejection(error)?,
    };
    let index = first_rejecting_prefix(connection, session_id, events)?;
    let mut total_events = index;
    let prefix_replay = replay_through(
        connection,
        session_id,
        &events[..index],
        0,
        &mut total_events,
        false,
        None,
    );
    let cause = match prefix_replay {
        Err(error) => inspection_rejection(error)?,
        Ok(_) => error,
    };
    Ok(Some(InspectionFailure {
        sequence: events[index - 1].sequence,
        cause: InspectionCause::Replay(cause),
    }))
}

fn inspection_rejection(error: StoreError) -> Result<crate::session::ReplayError, StoreError> {
    match error {
        StoreError::InvalidHistory => Ok(crate::session::ReplayError::InvalidTransition),
        StoreError::InvalidSnapshot => Ok(crate::session::ReplayError::InvalidSnapshot),
        other => Err(other),
    }
}

fn first_rejecting_prefix(
    connection: &Connection,
    session_id: SessionId,
    events: &[EventEnvelope],
) -> Result<usize, StoreError> {
    let (mut low, mut high) = (1, events.len());
    while low < high {
        let middle = low + (high - low) / 2;
        let mut total_events = middle;
        if let Err(error) = replay_through(
            connection,
            session_id,
            &events[..middle],
            0,
            &mut total_events,
            false,
            None,
        ) {
            inspection_rejection(error)?;
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    Ok(low)
}

pub(super) fn session_for_run(
    connection: &Connection,
    run_id: RunId,
) -> Result<Option<SessionId>, StoreError> {
    if connection.is_autocommit() {
        validate_history(connection)?;
    }
    let mut statement = connection.prepare(RUN_OWNER_SQL)?;
    let mut rows = statement.query([run_id.to_string()])?;
    let owner = rows
        .next()?
        .map(|row| {
            let raw = row
                .get_ref(0)?
                .as_str()
                .map_err(|_| StoreError::InvalidHistory)?;
            if raw.len() != 36 {
                return Err(StoreError::InvalidHistory);
            }
            raw.parse().map_err(|_| StoreError::InvalidHistory)
        })
        .transpose()?;
    if rows.next()?.is_some() {
        return Err(StoreError::InvalidHistory);
    }
    Ok(owner)
}

pub(super) fn latest_session_id(
    connection: &Connection,
    workspace_identity: WorkspaceIdentity,
) -> Result<Option<SessionId>, StoreError> {
    let heads = scan_session_heads(connection, &[])?;
    let mut latest = None::<(SessionId, u64)>;
    for (&id, head) in &heads {
        if discovered_identity(id, &heads, 0)? == Some(workspace_identity)
            && latest.is_none_or(|(_, sequence)| head.last_sequence > sequence)
        {
            latest = Some((id, head.last_sequence));
        }
    }
    let Some((id, _)) = latest else {
        return Ok(None);
    };
    let view = resolve_view(connection, id, 0, &mut 0)?
        .1
        .ok_or(StoreError::InvalidHistory)?;
    if view.workspace_identity != Some(workspace_identity) {
        return Err(StoreError::InvalidHistory);
    }
    Ok(Some(id))
}

pub(super) fn list_session_page(
    connection: &Connection,
    workspace_identity: WorkspaceIdentity,
    after: Option<(SessionId, u64)>,
    limit: usize,
    budget: usize,
    watermark: Option<u64>,
) -> Result<(SessionPage, u64), StoreError> {
    list_page_in_snapshot(
        connection,
        workspace_identity,
        after,
        limit,
        budget,
        watermark,
        || {},
    )
}

pub(super) fn list_page_in_snapshot(
    connection: &Connection,
    workspace_identity: WorkspaceIdentity,
    after: Option<(SessionId, u64)>,
    limit: usize,
    budget: usize,
    watermark: Option<u64>,
    after_ordering: impl FnOnce(),
) -> Result<(SessionPage, u64), StoreError> {
    let listing = discover_session_heads(connection, workspace_identity, watermark)?;
    let start = match after {
        Some((id, sequence)) => {
            listing
                .matching
                .iter()
                .position(|(tail, candidate, _)| *candidate == id && *tail == sequence)
                .ok_or(StoreError::StaleCursor)?
                + 1
        }
        None => 0,
    };
    after_ordering();
    let page = listing.page(connection, start, limit, budget)?;
    Ok((page, listing.watermark))
}

pub(super) fn list_all_sessions(
    connection: &Connection,
    workspace_identity: WorkspaceIdentity,
) -> Result<Vec<SessionListItem>, StoreError> {
    let listing = discover_session_heads(connection, workspace_identity, None)?;
    let mut items = Vec::new();
    loop {
        let page = listing.page(connection, items.len(), 100, MAX_LIST_SCAN_EVENTS)?;
        items.extend(page.items);
        if !page.more {
            return Ok(items);
        }
    }
}

struct SessionListing {
    workspace_identity: WorkspaceIdentity,
    watermark: u64,
    tails: HashMap<SessionId, u64>,
    matching: Vec<(u64, SessionId, Vec<SessionId>)>,
}

fn discover_session_heads(
    connection: &Connection,
    workspace_identity: WorkspaceIdentity,
    watermark: Option<u64>,
) -> Result<SessionListing, StoreError> {
    let watermark = retry_listing_read(|| match watermark {
        Some(sequence) => i64::try_from(sequence).map_err(|_| StoreError::InvalidHistory),
        None => Ok(connection.query_row(WATERMARK_SQL, [], |row| row.get(0))?),
    })?;
    retry_listing_read(|| discover_session_heads_at(connection, workspace_identity, watermark))
}

fn retry_listing_read<T>(mut read: impl FnMut() -> Result<T, StoreError>) -> Result<T, StoreError> {
    let busy = |result: &Result<T, StoreError>| {
        matches!(result, Err(StoreError::Sqlite(error))
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy))
    };
    let mut result = read();
    for _ in 0..MAX_LIST_BUSY_RETRIES {
        if !busy(&result) {
            return result;
        }
        result = read();
    }
    if busy(&result) {
        return Err(StoreError::ListingBusy);
    }
    result
}

fn discover_session_heads_at(
    connection: &Connection,
    workspace_identity: WorkspaceIdentity,
    watermark: i64,
) -> Result<SessionListing, StoreError> {
    let mut tails = HashMap::<SessionId, u64>::new();
    let mut roots = HashMap::<SessionId, (Option<WorkspaceIdentity>, Option<SessionId>)>::new();
    let mut next_head = connection.prepare(NEXT_HEAD_SQL)?;
    let mut head_tail = connection.prepare(HEAD_TAIL_SQL)?;
    let mut head_first = connection.prepare(HEAD_FIRST_SQL)?;
    let mut last = String::new();
    let mut exhausted = false;
    while !exhausted {
        let mut batch = Vec::<(String, i64, RawRow)>::new();
        while batch.len() < LIST_HEAD_CHUNK {
            let Some(raw_id) = next_head
                .query_row([&last], |row| row.get::<_, String>(0))
                .optional()?
            else {
                exhausted = true;
                break;
            };
            last.clone_from(&raw_id);
            let Some(tail) = head_tail
                .query_row(rusqlite::params![raw_id, watermark], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?
            else {
                continue;
            };
            let first = head_first
                .query_row(rusqlite::params![raw_id, watermark], |row| Ok(raw_row(row)))??;
            batch.push((raw_id, tail, first));
        }
        before_decode();
        for (raw_id, tail, raw) in batch {
            if tails.len() == MAX_DISCOVERED_SESSIONS {
                return Err(StoreError::TooManySessions);
            }
            let id = SessionId::from_str(&raw_id).map_err(|_| StoreError::InvalidHistory)?;
            tails.insert(
                id,
                u64::try_from(tail).map_err(|_| StoreError::InvalidHistory)?,
            );
            let envelope = raw.decode()?;
            let root = match envelope.event {
                Event::SessionStarted {
                    workspace_identity, ..
                } => (Some(workspace_identity), None),
                Event::SessionForked {
                    source_session_id, ..
                } if source_session_id != id => (None, Some(source_session_id)),
                _ => return Err(StoreError::InvalidHistory),
            };
            roots.insert(id, root);
        }
    }
    let lineage = |id: SessionId| -> Result<Vec<SessionId>, StoreError> {
        let mut chain = vec![id];
        while let (_, Some(source)) = *roots
            .get(chain.last().expect("lineage"))
            .ok_or(StoreError::InvalidHistory)?
        {
            if chain.len() > MAX_FORK_DEPTH {
                return Err(StoreError::ReplayLimit);
            }
            chain.push(source);
        }
        Ok(chain)
    };
    let mut matching = Vec::new();
    for (&id, &tail) in &tails {
        let chain = lineage(id)?;
        let root = chain.last().expect("lineage");
        if roots.get(root).and_then(|(identity, _)| *identity) == Some(workspace_identity) {
            matching.push((tail, id, chain));
        }
    }
    matching.sort_unstable_by_key(|(tail, ..)| std::cmp::Reverse(*tail));
    Ok(SessionListing {
        workspace_identity,
        watermark: u64::try_from(watermark).map_err(|_| StoreError::InvalidHistory)?,
        tails,
        matching,
    })
}

impl SessionListing {
    fn page(
        &self,
        connection: &Connection,
        start: usize,
        limit: usize,
        budget: usize,
    ) -> Result<SessionPage, StoreError> {
        retry_listing_read(|| self.page_once(connection, start, limit, budget))
    }

    fn page_once(
        &self,
        connection: &Connection,
        start: usize,
        limit: usize,
        mut budget: usize,
    ) -> Result<SessionPage, StoreError> {
        let mut timestamp =
            connection.prepare("SELECT strftime('%Y-%m-%d %H:%M:%S', ?1 / 1000, 'unixepoch')")?;
        let mut format_timestamp = |millis: i64| -> Result<String, StoreError> {
            Ok(timestamp
                .query_row([millis], |row| row.get::<_, Option<String>>(0))?
                .unwrap_or_else(|| "Unknown".into()))
        };
        let mut items = Vec::new();
        let mut more = false;
        for (_, id, chain) in self.matching.iter().skip(start) {
            if items.len() == limit {
                more = true;
                break;
            }
            let bounded = chain
                .iter()
                .map(|&member| {
                    self.tails
                        .get(&member)
                        .map(|&tail| (member, tail))
                        .ok_or(StoreError::InvalidHistory)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let passes = if chain.len() > 1 { 2 } else { 1 };
            let cost = lineage_event_count(connection, &bounded)?.saturating_mul(passes);
            if !items.is_empty() && cost > budget {
                more = true;
                break;
            }
            budget = budget.saturating_sub(cost);
            let mut heads = scan_session_heads(connection, &bounded)?;
            inherit_fork_metadata(connection, &bounded, &mut heads)?;
            let head = heads.get(id).ok_or(StoreError::InvalidHistory)?;
            if discovered_identity(*id, &heads, 0)? != Some(self.workspace_identity) {
                return Err(StoreError::InvalidHistory);
            }
            items.push(SessionListItem {
                id: *id,
                title: if head.explicit_title {
                    head.title.clone()
                } else {
                    head.fallback_title
                        .as_ref()
                        .map(|(_, title)| title.clone())
                        .unwrap_or_else(|| "Empty conversation".into())
                },
                last_sequence: head.last_sequence,
                created_at: format_timestamp(head.created_at_ms)?,
                last_activity_at: format_timestamp(head.last_activity_at_ms)?,
                defaults: if head.defaults.provider.is_some() {
                    head.defaults.clone()
                } else {
                    head.run_defaults.clone()
                },
            });
        }
        Ok(SessionPage { items, more })
    }
}

fn lineage_event_count(
    connection: &Connection,
    bounded: &[(SessionId, u64)],
) -> Result<usize, StoreError> {
    let mut total = 0_usize;
    for &(id, tail) in bounded {
        let count: i64 = connection.query_row(
            LINEAGE_COUNT_SQL,
            rusqlite::params![
                id.to_string(),
                i64::try_from(tail).map_err(|_| StoreError::InvalidHistory)?
            ],
            |row| row.get(0),
        )?;
        total =
            total.saturating_add(usize::try_from(count).map_err(|_| StoreError::InvalidHistory)?);
    }
    Ok(total)
}

fn scan_session_heads(
    connection: &Connection,
    filter: &[(SessionId, u64)],
) -> Result<HashMap<SessionId, SessionHead>, StoreError> {
    let mut heads = HashMap::<SessionId, SessionHead>::new();
    scan_events(connection, filter, |envelope| {
        let id = envelope.session_id;
        if let Some(head) = heads.get_mut(&id) {
            if matches!(
                envelope.event,
                Event::SessionStarted { .. } | Event::SessionForked { .. }
            ) {
                return Err(StoreError::InvalidHistory);
            }
            if let Event::RunStarted { config, .. } = &envelope.event {
                head.run_defaults = selection_from_run(config);
                let identity = WorkspaceIdentity {
                    device: config.workspace_device,
                    inode: config.workspace_inode,
                };
                if head
                    .direct_identity
                    .is_some_and(|pinned| pinned != identity)
                {
                    return Err(StoreError::InvalidHistory);
                }
                head.direct_identity = Some(identity);
            }
            if let Event::SessionRenamed { title } = &envelope.event {
                head.title.clone_from(title);
                head.explicit_title = true;
            }
            match &envelope.event {
                Event::SessionDefaultChanged { defaults } => head.defaults = defaults.clone(),
                Event::MessageAccepted { text, .. } if head.fallback_title.is_none() => {
                    head.fallback_title =
                        Some((envelope.sequence, crate::session::title_preview(text)));
                }
                _ => {}
            }
            head.last_activity_at_ms = envelope.created_at_ms;
            head.last_sequence = envelope.sequence;
        } else {
            if heads.len() == MAX_DISCOVERED_SESSIONS {
                return Err(StoreError::TooManySessions);
            }
            let (title, direct_identity, source_session_id) = match &envelope.event {
                Event::SessionStarted {
                    title,
                    workspace_identity,
                } => (title.clone(), Some(*workspace_identity), None),
                Event::SessionForked {
                    title,
                    source_session_id,
                    ..
                } if *source_session_id != id => (title.clone(), None, Some(*source_session_id)),
                _ => return Err(StoreError::InvalidHistory),
            };
            heads.insert(
                id,
                SessionHead {
                    explicit_title: !crate::session::placeholder_title(&title),
                    title,
                    created_at_ms: envelope.created_at_ms,
                    last_activity_at_ms: envelope.created_at_ms,
                    fallback_title: None,
                    source_sequence: match &envelope.event {
                        Event::SessionForked {
                            source_sequence, ..
                        } => Some(*source_sequence),
                        _ => None,
                    },
                    defaults: SessionDefaults::default(),
                    run_defaults: SessionDefaults::default(),
                    direct_identity,
                    source_session_id,
                    last_sequence: envelope.sequence,
                },
            );
        }
        Ok(())
    })?;
    Ok(heads)
}

fn selection_from_run(config: &RunConfig) -> SessionDefaults {
    SessionDefaults {
        provider: Some(config.provider.profile()),
        model: Some(config.model.clone()),
        effort: config.effort,
        account_id: config.saved_api_account_id.or_else(|| {
            config
                .chatgpt_provenance
                .as_ref()
                .map(|source| source.account_id)
        }),
        policy: config.policy,
    }
}

fn inherit_fork_metadata(
    connection: &Connection,
    filter: &[(SessionId, u64)],
    heads: &mut HashMap<SessionId, SessionHead>,
) -> Result<(), StoreError> {
    let mut boundaries = heads
        .iter()
        .filter_map(|(&id, head)| Some((head.source_sequence?, head.source_session_id?, id)))
        .collect::<Vec<_>>();
    if boundaries.is_empty() {
        return Ok(());
    }
    boundaries.sort_unstable_by_key(|(sequence, _, _)| *sequence);
    let mut next = 0;
    let mut selections = HashMap::<SessionId, SessionDefaults>::new();
    let mut inherited = HashMap::<SessionId, (SessionDefaults, Option<(u64, String)>)>::new();
    scan_events(connection, filter, |envelope| {
        match &envelope.event {
            Event::RunStarted { config, .. } => {
                selections.insert(envelope.session_id, selection_from_run(config));
            }
            Event::SessionForked { .. } => {
                if let Some((selection, _)) = inherited.get(&envelope.session_id) {
                    selections.insert(envelope.session_id, selection.clone());
                }
            }
            _ => {}
        }
        while let Some(&(sequence, source, fork)) = boundaries.get(next) {
            if sequence > envelope.sequence {
                break;
            }
            let source_head = heads.get(&source).ok_or(StoreError::InvalidHistory)?;
            let title = source_head
                .fallback_title
                .as_ref()
                .filter(|(title_sequence, _)| *title_sequence <= sequence)
                .cloned()
                .or_else(|| inherited.get(&source).and_then(|(_, title)| title.clone()));
            inherited.insert(
                fork,
                (selections.get(&source).cloned().unwrap_or_default(), title),
            );
            next += 1;
        }
        Ok(())
    })?;
    for (id, (selection, title)) in inherited {
        let head = heads.get_mut(&id).ok_or(StoreError::InvalidHistory)?;
        if head.run_defaults.provider.is_none() {
            head.run_defaults = selection;
        }
        if head.fallback_title.is_none() {
            head.fallback_title = title;
        }
    }
    Ok(())
}

fn discovered_identity(
    id: SessionId,
    heads: &HashMap<SessionId, SessionHead>,
    depth: usize,
) -> Result<Option<WorkspaceIdentity>, StoreError> {
    if depth > MAX_FORK_DEPTH {
        return Err(StoreError::ReplayLimit);
    }
    let head = heads.get(&id).ok_or(StoreError::InvalidHistory)?;
    let Some(source_id) = head.source_session_id else {
        return Ok(head.direct_identity);
    };
    let inherited =
        discovered_identity(source_id, heads, depth + 1)?.ok_or(StoreError::InvalidHistory)?;
    if head
        .direct_identity
        .is_some_and(|identity| identity != inherited)
    {
        return Err(StoreError::InvalidHistory);
    }
    Ok(Some(inherited))
}

pub(super) fn resolve_view(
    connection: &Connection,
    session_id: SessionId,
    depth: usize,
    total_events: &mut usize,
) -> Result<ResolvedSession, StoreError> {
    resolve_view_with_mode(connection, session_id, depth, total_events, true)
}

pub(super) fn resolve_open_view(
    connection: &Connection,
    session_id: SessionId,
    total_events: &mut usize,
) -> Result<ResolvedSession, StoreError> {
    resolve_view_with_mode(connection, session_id, 0, total_events, false)
}

fn resolve_view_with_mode(
    connection: &Connection,
    session_id: SessionId,
    depth: usize,
    total_events: &mut usize,
    close_history: bool,
) -> Result<ResolvedSession, StoreError> {
    if depth > MAX_FORK_DEPTH {
        return Err(StoreError::ReplayLimit);
    }
    let events = load_session_events(connection, session_id)?;
    *total_events = total_events
        .checked_add(events.len())
        .ok_or(StoreError::ReplayLimit)?;
    if *total_events > MAX_SESSION_EVENTS {
        return Err(StoreError::ReplayLimit);
    }
    let view = replay_through(
        connection,
        session_id,
        &events,
        depth,
        total_events,
        close_history,
        None,
    )?;
    Ok((events, view))
}

pub(super) fn replay_through(
    connection: &Connection,
    session_id: SessionId,
    events: &[EventEnvelope],
    depth: usize,
    total_events: &mut usize,
    close_history: bool,
    corpus: Option<&mut crate::supervisor::corpus::Corpus>,
) -> Result<Option<SessionView>, StoreError> {
    let lineage = match events.first().map(|envelope| &envelope.event) {
        Some(Event::SessionForked {
            title: _,
            source_session_id,
            source_run_id,
            source_sequence,
            prefix_digest,
        }) if *source_session_id != session_id => ForkLineage {
            source_session_id: *source_session_id,
            source_run_id: *source_run_id,
            source_sequence: *source_sequence,
            prefix_digest: *prefix_digest,
        },
        _ => {
            let view = if let Some(corpus) = corpus {
                SessionView::replay_for_corpus(session_id, events, None, corpus)?
            } else if close_history {
                SessionView::replay(session_id, events)?
            } else {
                SessionView::replay_open(session_id, events)?
            };
            if let Some(view) = &view {
                validate_compaction_references(view)?;
            }
            return Ok(view);
        }
    };
    let (inherited, source) = fork_projection(connection, lineage, depth, total_events)?;
    let mut view = if let Some(corpus) = corpus {
        SessionView::replay_for_corpus(session_id, events, Some(&inherited), corpus)?
    } else {
        SessionView::replay_forked(session_id, events, close_history, &inherited)?
    }
    .ok_or(StoreError::InvalidHistory)?;
    if view
        .workspace_identity
        .is_some_and(|identity| Some(identity) != source.workspace_identity)
    {
        return Err(StoreError::InvalidHistory);
    }
    view.workspace_identity = source.workspace_identity;
    view.inherited_title = Some(source.generated_title_through(lineage.source_sequence));
    validate_compaction_references(&view)?;
    Ok(Some(view))
}

fn fork_projection(
    connection: &Connection,
    fork: ForkLineage,
    depth: usize,
    total_events: &mut usize,
) -> Result<(ForkInheritance, SessionView), StoreError> {
    if depth + 1 > MAX_FORK_DEPTH {
        return Err(StoreError::ReplayLimit);
    }
    let source_events =
        load_session_events_through(connection, fork.source_session_id, fork.source_sequence)?;
    *total_events = total_events
        .checked_add(source_events.len())
        .ok_or(StoreError::ReplayLimit)?;
    if *total_events > MAX_SESSION_EVENTS {
        return Err(StoreError::ReplayLimit);
    }
    let boundary = source_events
        .iter()
        .find(|event| event.sequence == fork.source_sequence)
        .ok_or(StoreError::InvalidHistory)?;
    match &boundary.event {
        Event::RunFinished { run_id, .. }
        | Event::SessionForked {
            source_run_id: run_id,
            ..
        } if *run_id == fork.source_run_id => {}
        _ => return Err(StoreError::InvalidHistory),
    }
    if prefix_digest(&source_events, fork.source_sequence)? != fork.prefix_digest {
        return Err(StoreError::InvalidHistory);
    }
    let source = replay_through(
        connection,
        fork.source_session_id,
        &source_events,
        depth + 1,
        total_events,
        false,
        None,
    )?
    .ok_or(StoreError::InvalidHistory)?;
    let inherited = ForkInheritance {
        usage: source.usage.clone(),
        workspace_identity: source.workspace_identity,
        rewinds: source.rewinds.clone(),
        runs: source
            .runs
            .iter()
            .filter(|run| {
                run.finished_sequence
                    .is_some_and(|sequence| sequence <= fork.source_sequence)
            })
            .cloned()
            .collect(),
        compactions: source.compactions.clone(),
    };
    if inherited
        .runs
        .last()
        .is_none_or(|run| run.id != fork.source_run_id)
    {
        return Err(StoreError::InvalidHistory);
    }
    Ok((inherited, source))
}

fn load_session_events(
    connection: &Connection,
    session_id: SessionId,
) -> Result<Vec<EventEnvelope>, StoreError> {
    let mut selected = Vec::new();
    if connection.is_autocommit() {
        scan_all_events(connection, |envelope| {
            if envelope.session_id == session_id {
                if selected.len() == MAX_SESSION_EVENTS {
                    return Err(StoreError::ReplayLimit);
                }
                selected.push(envelope);
            }
            Ok(())
        })?;
    } else {
        let mut statement = connection.prepare(SESSION_EVENTS_SQL)?;
        let mut rows = statement.query(rusqlite::params![
            session_id.to_string(),
            (MAX_SESSION_EVENTS + 1) as i64,
        ])?;
        while let Some(row) = rows.next()? {
            if selected.len() == MAX_SESSION_EVENTS {
                return Err(StoreError::ReplayLimit);
            }
            selected.push(decode_event(row)?);
        }
    }
    Ok(selected)
}

fn load_session_events_through(
    connection: &Connection,
    session_id: SessionId,
    sequence: u64,
) -> Result<Vec<EventEnvelope>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT sequence,
            length(CAST(session_id AS BLOB)),
            length(CAST(kind AS BLOB)),
            length(CAST(payload AS BLOB)),
            session_id, run_id, agent_run_id, kind, event_version, payload, created_at_ms
         FROM events WHERE session_id = ?1 AND sequence <= ?2 ORDER BY sequence LIMIT ?3",
    )?;
    let mut rows = statement.query(rusqlite::params![
        session_id.to_string(),
        i64::try_from(sequence).map_err(|_| StoreError::InvalidHistory)?,
        (MAX_SESSION_EVENTS + 1) as i64,
    ])?;
    let mut selected = Vec::new();
    while let Some(row) = rows.next()? {
        if selected.len() == MAX_SESSION_EVENTS {
            return Err(StoreError::ReplayLimit);
        }
        selected.push(decode_event(row)?);
    }
    if selected.last().map(|event| event.sequence) != Some(sequence) {
        return Err(StoreError::InvalidHistory);
    }
    Ok(selected)
}

pub(super) fn validate_history(connection: &Connection) -> Result<(), StoreError> {
    super::check::validate(connection)?;
    scan_all_events(connection, |_| Ok(()))
}

fn scan_all_events(
    connection: &Connection,
    on_event: impl FnMut(EventEnvelope) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    scan_events(connection, &[], on_event)
}

fn scan_events(
    connection: &Connection,
    filter: &[(SessionId, u64)],
    mut on_event: impl FnMut(EventEnvelope) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    const COLUMNS: &str = "SELECT sequence,
                length(CAST(session_id AS BLOB)),
                length(CAST(kind AS BLOB)),
                length(CAST(payload AS BLOB)),
                session_id, run_id, agent_run_id, kind, event_version, payload, created_at_ms
         FROM events";
    if filter.is_empty() {
        // Cold or invalidated writes validate globally; unchanged cache hits defer detection and keep reads indexed.
        if !connection.is_autocommit() {
            return Err(StoreError::InvalidHistory);
        }
        let mut statement = connection.prepare(&format!("{COLUMNS} ORDER BY sequence"))?;
        let mut rows = statement.query([])?;
        let mut expected = 1_i64;
        while let Some(row) = rows.next()? {
            let sequence: i64 = row.get(0)?;
            if sequence != expected {
                return Err(StoreError::InvalidHistory);
            }
            expected = expected.checked_add(1).ok_or(StoreError::InvalidHistory)?;
            on_event(decode_event(row)?)?;
        }
        return Ok(());
    }
    struct Lane {
        id: String,
        bound: i64,
        after: i64,
        rows: std::collections::VecDeque<RawRow>,
        done: bool,
    }
    let mut lanes = filter
        .iter()
        .map(|(id, tail)| {
            Ok(Lane {
                id: id.to_string(),
                bound: i64::try_from(*tail).map_err(|_| StoreError::InvalidHistory)?,
                after: 0,
                rows: std::collections::VecDeque::new(),
                done: false,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    let mut statement = connection.prepare(LANE_SQL)?;
    loop {
        let mut fetched = false;
        for lane in lanes.iter_mut().filter(|l| l.rows.is_empty() && !l.done) {
            let mut rows = statement.query(rusqlite::params![lane.id, lane.after, lane.bound])?;
            while let Some(row) = rows.next()? {
                lane.rows.push_back(raw_row(row)?);
            }
            lane.done = lane.rows.len() < LIST_EVENT_CHUNK;
            lane.after = lane.rows.back().map_or(lane.after, |raw| raw.sequence);
            fetched = true;
        }
        if fetched {
            before_decode();
        }
        let Some(lane) = lanes
            .iter_mut()
            .filter(|l| !l.rows.is_empty())
            .min_by_key(|l| l.rows[0].sequence)
        else {
            return Ok(());
        };
        let raw = lane.rows.pop_front().expect("non-empty lane");
        on_event(raw.decode()?)?;
    }
}

pub(super) struct RawRow {
    pub(super) sequence: i64,
    id_bytes: i64,
    kind_bytes: i64,
    pub(super) payload_bytes: i64,
    pub(super) session_id: String,
    pub(super) run_id: Option<String>,
    pub(super) agent_run_id: Option<String>,
    pub(super) kind: String,
    pub(super) version: i64,
    pub(super) payload: String,
    pub(super) created_at_ms: i64,
}

pub(super) fn raw_row(row: &rusqlite::Row<'_>) -> Result<RawRow, StoreError> {
    let text = |index: usize| -> Result<String, StoreError> {
        row.get::<_, String>(index)
            .map_err(|_| StoreError::InvalidHistory)
    };
    let optional = |index: usize| -> Result<Option<String>, StoreError> {
        row.get::<_, Option<String>>(index)
            .map_err(|_| StoreError::InvalidHistory)
    };
    Ok(RawRow {
        sequence: row.get(0)?,
        id_bytes: row.get(1)?,
        kind_bytes: row.get(2)?,
        payload_bytes: row.get(3)?,
        session_id: text(4)?,
        run_id: optional(5)?,
        agent_run_id: optional(6)?,
        kind: text(7)?,
        version: row.get(8)?,
        payload: text(9)?,
        created_at_ms: row.get(10)?,
    })
}

impl RawRow {
    fn decode_inspected(self) -> Result<EventEnvelope, crate::session::ReplayError> {
        if self.id_bytes != 36
            || !(1..=MAX_EVENT_KIND_BYTES as i64).contains(&self.kind_bytes)
            || self.created_at_ms < 0
        {
            return Err(crate::session::ReplayError::InvalidPayload);
        }
        let id = SessionId::from_str(&self.session_id)
            .map_err(|_| crate::session::ReplayError::InvalidPayload)?;
        if self.run_id.as_ref().is_some_and(|raw| raw.len() != 36)
            || self
                .agent_run_id
                .as_ref()
                .is_some_and(|raw| raw.len() != 36)
        {
            return Err(crate::session::ReplayError::InvalidPayload);
        }
        let run_id = self
            .run_id
            .as_deref()
            .map(RunId::from_str)
            .transpose()
            .map_err(|_| crate::session::ReplayError::InvalidPayload)?;
        let agent_run_id = self
            .agent_run_id
            .as_deref()
            .map(AgentRunId::from_str)
            .transpose()
            .map_err(|_| crate::session::ReplayError::InvalidPayload)?;
        if !(1..=event_payload_limit(&self.kind, self.version) as i64).contains(&self.payload_bytes)
        {
            return Err(crate::session::ReplayError::InvalidPayload);
        }
        let event = Event::decode(
            &self.kind,
            self.version,
            &self.payload,
            run_id,
            agent_run_id,
        )?;
        Ok(EventEnvelope {
            sequence: u64::try_from(self.sequence)
                .map_err(|_| crate::session::ReplayError::InvalidPayload)?,
            session_id: id,
            run_id,
            agent_run_id,
            event,
            created_at_ms: self.created_at_ms,
        })
    }

    pub(super) fn decode(self) -> Result<EventEnvelope, StoreError> {
        self.decode_inspected().map_err(StoreError::from)
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static BEFORE_DECODE: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

fn before_decode() {
    #[cfg(test)]
    BEFORE_DECODE.with(|hook| {
        if let Some(hook) = &*hook.borrow() {
            hook();
        }
    });
}

fn decode_event(row: &Row<'_>) -> Result<EventEnvelope, StoreError> {
    raw_row(row)?.decode()
}

pub(super) fn supervisor_corpus(
    connection: &Connection,
    session_id: SessionId,
) -> Result<String, crate::supervisor::CorpusError> {
    let events = load_session_events(connection, session_id)?;
    let mut total = events.len();
    if total > MAX_SESSION_EVENTS {
        return Err(StoreError::ReplayLimit.into());
    }
    let mut corpus = crate::supervisor::corpus::Corpus::new()?;
    let view = replay_through(
        connection,
        session_id,
        &events,
        0,
        &mut total,
        true,
        Some(&mut corpus),
    )?
    .ok_or(crate::supervisor::CorpusError::NotFound)?;
    corpus.finish(&view)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_event_decode_requires_text_kind() {
        let temp = tempfile::tempdir().unwrap();
        let connection = Connection::open(temp.path().join("events.sqlite3")).unwrap();
        let session = SessionId::new();
        let event = Event::SessionRenamed {
            title: "Title".into(),
        };
        for (kind, accepted) in [
            (rusqlite::types::Value::Text(event.kind().into()), true),
            (
                rusqlite::types::Value::Blob(event.kind().as_bytes().to_vec()),
                false,
            ),
        ] {
            let result = connection
                .query_row(
                    "SELECT 1, 36, length(CAST(?1 AS BLOB)), length(CAST(?2 AS BLOB)), ?3, NULL, NULL, ?1, ?4, ?2, 0",
                    rusqlite::params![kind, event.payload().unwrap(), session.to_string(), event.version()],
                    |row| Ok(decode_event(row)),
                )
                .unwrap();
            if accepted {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(StoreError::InvalidHistory)));
            }
        }
    }

    #[test]
    fn strict_event_decode_rejects_noncanonical_envelope_id_lengths() {
        let session_id = SessionId::new();
        let run_id = RunId::new();
        let agent_run_id = AgentRunId::new();
        let event = Event::AgentSpawned {
            run_id,
            agent_run_id,
            role: crate::session::AgentRole::Primary,
            ordinal: 0,
            objective: None,
            grant: None,
            definition: None,
            profile: None,
            assignment: None,
        };
        let payload = event.payload().unwrap();
        let canonical = RawRow {
            sequence: 1,
            id_bytes: 36,
            kind_bytes: event.kind().len() as i64,
            payload_bytes: payload.len() as i64,
            session_id: session_id.to_string(),
            run_id: Some(run_id.to_string()),
            agent_run_id: Some(agent_run_id.to_string()),
            kind: event.kind().into(),
            version: event.version(),
            payload: payload.clone(),
            created_at_ms: 0,
        };
        assert!(canonical.decode().is_ok());
        for malformed_run in [true, false] {
            let mut raw = RawRow {
                sequence: 1,
                id_bytes: 36,
                kind_bytes: event.kind().len() as i64,
                payload_bytes: payload.len() as i64,
                session_id: session_id.to_string(),
                run_id: Some(run_id.to_string()),
                agent_run_id: Some(agent_run_id.to_string()),
                kind: event.kind().into(),
                version: event.version(),
                payload: payload.clone(),
                created_at_ms: 0,
            };
            if malformed_run {
                raw.run_id = Some(run_id.to_string().replace('-', ""));
            } else {
                raw.agent_run_id = Some(agent_run_id.to_string().replace('-', ""));
            }
            assert!(raw.decode_inspected().is_err());
        }
    }
}
