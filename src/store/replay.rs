use super::{ResolvedSession, StoreError, event_payload_limit};
use crate::session::{
    AgentDisposition, AgentRole, AgentRunId, Event, EventEnvelope, RunConfig, RunDisposition,
    RunId, SessionDefaults, SessionId, SessionListItem, SessionView, prefix_digest,
    validate_compaction_references,
};
use rusqlite::{Connection, types::ValueRef};
use std::collections::HashMap;
use std::str::FromStr;

pub(super) const MAX_SESSION_EVENTS: usize = 10_000;
const MAX_FORK_DEPTH: usize = 8;
const MAX_DISCOVERED_SESSIONS: usize = 65_536;

struct SessionHead {
    title: String,
    direct_identity: Option<(u64, u64)>,
    source_session_id: Option<SessionId>,
    source_sequence: Option<u64>,
    last_sequence: u64,
    created_at_ms: i64,
    last_activity_at_ms: i64,
    explicit_title: bool,
    fallback_title: Option<(u64, String)>,
    ai_title: Option<(u64, String)>,
    primary: Option<(RunId, AgentRunId)>,
    pending_title: Option<String>,
    defaults: SessionDefaults,
    run_defaults: SessionDefaults,
}

pub(super) fn load_session(
    connection: &Connection,
    session_id: SessionId,
) -> Result<Vec<EventEnvelope>, StoreError> {
    resolve_view(connection, session_id, 0, &mut 0).map(|(events, _)| events)
}

pub(super) fn session_for_run(
    connection: &Connection,
    run_id: RunId,
) -> Result<Option<SessionId>, StoreError> {
    let mut selected = None;
    scan_all_events(connection, |envelope| {
        if envelope.run_id == Some(run_id) {
            if selected.is_some_and(|id| id != envelope.session_id) {
                return Err(StoreError::InvalidHistory);
            }
            selected = Some(envelope.session_id);
        }
        Ok(())
    })?;
    Ok(selected)
}

pub(super) fn latest_session_id(
    connection: &Connection,
    workspace_identity: (u64, u64),
) -> Result<Option<SessionId>, StoreError> {
    let heads = scan_session_heads(connection)?;
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

pub(super) fn list_session_items(
    connection: &Connection,
    workspace_identity: (u64, u64),
) -> Result<Vec<SessionListItem>, StoreError> {
    let mut heads = scan_session_heads(connection)?;
    inherit_fork_metadata(connection, &mut heads)?;
    let mut items = Vec::new();
    let mut timestamp =
        connection.prepare("SELECT strftime('%Y-%m-%d %H:%M:%S', ?1 / 1000, 'unixepoch')")?;
    let mut format_timestamp = |millis: i64| -> Result<String, StoreError> {
        Ok(timestamp
            .query_row([millis], |row| row.get::<_, Option<String>>(0))?
            .unwrap_or_else(|| "Unknown".into()))
    };
    for (&id, head) in &heads {
        if discovered_identity(id, &heads, 0)? == Some(workspace_identity) {
            items.push(SessionListItem {
                id,
                title: if head.explicit_title {
                    head.title.clone()
                } else {
                    head.ai_title
                        .as_ref()
                        .map(|(_, title)| title.clone())
                        .or_else(|| head.fallback_title.as_ref().map(|(_, title)| title.clone()))
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
    }
    items.sort_unstable_by_key(|item| std::cmp::Reverse(item.last_sequence));
    Ok(items)
}

fn scan_session_heads(
    connection: &Connection,
) -> Result<HashMap<SessionId, SessionHead>, StoreError> {
    let mut heads = HashMap::<SessionId, SessionHead>::new();
    scan_all_events(connection, |envelope| {
        let id = envelope.session_id;
        if let Some(head) = heads.get_mut(&id) {
            if matches!(
                envelope.event,
                Event::SessionStarted { .. } | Event::SessionForked { .. }
            ) {
                return Err(StoreError::InvalidHistory);
            }
            if let Event::RunStarted { config, .. } = &envelope.event {
                head.primary = None;
                head.pending_title = None;
                head.run_defaults = selection_from_run(config);
                let identity = (config.workspace_device, config.workspace_inode);
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
                Event::AgentSpawned {
                    run_id,
                    agent_run_id,
                    role: AgentRole::Primary,
                    ..
                } => {
                    head.primary = Some((*run_id, *agent_run_id));
                }
                Event::AgentFinished {
                    run_id,
                    agent_run_id,
                    disposition: AgentDisposition::Finished,
                    summary: Some(summary),
                    ..
                } if head.primary == Some((*run_id, *agent_run_id)) && head.ai_title.is_none() => {
                    head.pending_title = Some(crate::session::title_preview(summary));
                }
                Event::RunFinished {
                    run_id,
                    disposition: RunDisposition::Finished,
                } if head
                    .primary
                    .is_some_and(|(primary_run, _)| primary_run == *run_id) =>
                {
                    if let Some(title) = head.pending_title.take() {
                        head.ai_title = Some((envelope.sequence, title));
                    }
                }
                Event::RunFinished { .. } => head.pending_title = None,
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
                } => (title.clone(), *workspace_identity, None),
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
                    explicit_title: !matches!(title.as_str(), "New Session" | "Forked Session"),
                    title,
                    created_at_ms: envelope.created_at_ms,
                    last_activity_at_ms: envelope.created_at_ms,
                    fallback_title: None,
                    ai_title: None,
                    source_sequence: match &envelope.event {
                        Event::SessionForked {
                            source_sequence, ..
                        } => Some(*source_sequence),
                        _ => None,
                    },
                    primary: None,
                    pending_title: None,
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
        provider: Some(config.provider.clone()),
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
    scan_all_events(connection, |envelope| {
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
                .ai_title
                .as_ref()
                .filter(|(title_sequence, _)| *title_sequence <= sequence)
                .cloned()
                .or_else(|| {
                    source_head
                        .fallback_title
                        .as_ref()
                        .filter(|(title_sequence, _)| *title_sequence <= sequence)
                        .cloned()
                })
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
) -> Result<Option<(u64, u64)>, StoreError> {
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
    let mut view = if close_history {
        SessionView::replay(session_id, &events)?
    } else {
        SessionView::replay_open(session_id, &events)?
    };
    if let Some(current) = &mut view
        && let Some(lineage) = current.lineage
    {
        let (source_events, source_view) = resolve_view(
            connection,
            lineage.source_session_id,
            depth + 1,
            total_events,
        )?;
        let source = source_view.ok_or(StoreError::InvalidHistory)?;
        if current.workspace_identity.is_some()
            && current.workspace_identity != source.workspace_identity
        {
            return Err(StoreError::InvalidHistory);
        }
        current.workspace_identity = source.workspace_identity;
        let boundary = source_events
            .iter()
            .find(|event| event.sequence == lineage.source_sequence)
            .ok_or(StoreError::InvalidHistory)?;
        match &boundary.event {
            Event::RunFinished { run_id, .. }
            | Event::SessionForked {
                source_run_id: run_id,
                ..
            } if *run_id == lineage.source_run_id => {}
            _ => return Err(StoreError::InvalidHistory),
        }
        if prefix_digest(&source_events, lineage.source_sequence)? != lineage.prefix_digest {
            return Err(StoreError::InvalidHistory);
        }
        current.inherited_title = Some(source.generated_title_through(lineage.source_sequence));
        let inherited: Vec<_> = source
            .runs
            .into_iter()
            .filter(|run| {
                run.finished_sequence
                    .is_some_and(|sequence| sequence <= lineage.source_sequence)
            })
            .collect();
        if inherited
            .last()
            .is_none_or(|run| run.id != lineage.source_run_id)
            || current
                .runs
                .iter()
                .any(|run| inherited.iter().any(|parent| parent.id == run.id))
        {
            return Err(StoreError::InvalidHistory);
        }
        current.runs.splice(0..0, inherited);
        let inherited_compactions = source
            .compactions
            .into_iter()
            .filter(|compaction| compaction.event_sequence <= lineage.source_sequence);
        current.compactions.splice(0..0, inherited_compactions);
    }
    if let Some(current) = &view {
        validate_compaction_references(current)?;
    }
    Ok((events, view))
}

fn load_session_events(
    connection: &Connection,
    session_id: SessionId,
) -> Result<Vec<EventEnvelope>, StoreError> {
    let mut selected = Vec::new();
    scan_all_events(connection, |envelope| {
        if envelope.session_id == session_id {
            if selected.len() == MAX_SESSION_EVENTS {
                return Err(StoreError::ReplayLimit);
            }
            selected.push(envelope);
        }
        Ok(())
    })?;
    Ok(selected)
}

fn scan_all_events(
    connection: &Connection,
    mut on_event: impl FnMut(EventEnvelope) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT sequence,
                length(CAST(session_id AS BLOB)),
                length(CAST(kind AS BLOB)),
                length(CAST(payload AS BLOB)),
                session_id, run_id, agent_run_id, kind, event_version, payload, created_at_ms
         FROM events ORDER BY sequence",
    )?;
    let mut rows = statement.query([])?;
    let mut expected = 1_i64;
    while let Some(row) = rows.next()? {
        let sequence: i64 = row.get(0)?;
        if sequence != expected {
            return Err(StoreError::InvalidHistory);
        }
        expected = expected.checked_add(1).ok_or(StoreError::InvalidHistory)?;
        let id_bytes: i64 = row.get(1)?;
        let kind_bytes: i64 = row.get(2)?;
        let payload_bytes: i64 = row.get(3)?;
        if id_bytes != 36 || !(1..=32).contains(&kind_bytes) {
            return Err(StoreError::InvalidHistory);
        }
        let raw_id = row
            .get_ref(4)?
            .as_str()
            .map_err(|_| StoreError::InvalidHistory)?;
        let id = SessionId::from_str(raw_id).map_err(|_| StoreError::InvalidHistory)?;
        let run_id = match row.get_ref(5)? {
            ValueRef::Null => None,
            ValueRef::Text(raw) if raw.len() == 36 => Some(
                RunId::from_str(std::str::from_utf8(raw).map_err(|_| StoreError::InvalidHistory)?)
                    .map_err(|_| StoreError::InvalidHistory)?,
            ),
            _ => return Err(StoreError::InvalidHistory),
        };
        let agent_run_id = match row.get_ref(6)? {
            ValueRef::Null => None,
            ValueRef::Text(raw) if raw.len() == 36 => Some(
                AgentRunId::from_str(
                    std::str::from_utf8(raw).map_err(|_| StoreError::InvalidHistory)?,
                )
                .map_err(|_| StoreError::InvalidHistory)?,
            ),
            _ => return Err(StoreError::InvalidHistory),
        };
        let kind = row
            .get_ref(7)?
            .as_str()
            .map_err(|_| StoreError::InvalidHistory)?;
        let version: i64 = row.get(8)?;
        if !(1..=event_payload_limit(kind, version) as i64).contains(&payload_bytes) {
            return Err(StoreError::InvalidHistory);
        }
        let payload = row
            .get_ref(9)?
            .as_str()
            .map_err(|_| StoreError::InvalidHistory)?;
        let event = Event::decode(kind, version, payload, run_id, agent_run_id)?;
        let created_at_ms: i64 = row.get(10)?;
        if created_at_ms < 0 {
            return Err(StoreError::InvalidHistory);
        }
        on_event(EventEnvelope {
            sequence: u64::try_from(sequence).map_err(|_| StoreError::InvalidHistory)?,
            session_id: id,
            run_id,
            agent_run_id,
            event,
            created_at_ms,
        })?;
    }
    Ok(())
}
