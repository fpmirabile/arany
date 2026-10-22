use super::{StoreError, evidence};
use crate::journal::check::{
    CHECK_FORMATS, CheckDisposition, CheckEvent, CheckId, CheckOp, CheckReducer, CheckTarget,
    CheckView, MAX_CHECKS, MAX_EVENTS, MAX_PAYLOAD, now_ms,
};
use crate::provider::evidence::{NativeEvidenceRecord, ProviderEvidenceRecord};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub(super) fn apply(
    connection: &mut Connection,
    read_only: bool,
    op: CheckOp,
) -> Result<Option<CheckView>, StoreError> {
    if let CheckOp::Load(id) = op {
        return load(connection, id);
    }
    if read_only {
        return Err(StoreError::ReadOnly);
    }
    if matches!(op, CheckOp::Recover) {
        super::replay::validate_history(connection)?;
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let result = match op {
        #[cfg(test)]
        CheckOp::Seed(record) => Some(seed(&transaction, record)?),
        CheckOp::Load(_) => return Err(StoreError::InvalidHistory),
        CheckOp::Recover => {
            for view in views(&transaction)?
                .into_values()
                .filter(|view| view.disposition.is_none())
            {
                if let Some((ordinal, _)) = view.pending {
                    append(
                        &transaction,
                        view.id,
                        CheckEvent::ProviderCheckCallFinished {
                            ordinal,
                            disposition: CheckDisposition::Uncertain,
                            usage: None,
                            catalog_complete: false,
                        },
                    )?;
                }
                append(
                    &transaction,
                    view.id,
                    CheckEvent::ProviderCheckFinished {
                        disposition: CheckDisposition::Uncertain,
                        evidence: None,
                    },
                )?;
            }
            None
        }
        CheckOp::Start { id, start } => {
            let existing = views(&transaction)?;
            if existing.values().any(|view| view.disposition.is_none()) {
                return Err(StoreError::ProviderCheckBusy);
            }
            if existing.len() >= MAX_CHECKS {
                let oldest: Option<String> = transaction.query_row(
                    "SELECT check_id FROM provider_check_events AS checks
                     WHERE kind='ProviderCheckFinished'
                     AND NOT EXISTS (SELECT 1 FROM provider_evidence WHERE check_id=checks.check_id)
                     AND NOT EXISTS (SELECT 1 FROM native_provider_evidence WHERE check_id=checks.check_id)
                     ORDER BY sequence LIMIT 1", [], |row| row.get(0)).optional()?;
                let oldest = oldest.ok_or(StoreError::StorageFull)?;
                transaction.execute(
                    "DELETE FROM provider_check_events WHERE check_id=?1",
                    [oldest],
                )?;
            }
            let page_count: i64 =
                transaction.query_row("PRAGMA page_count", [], |row| row.get(0))?;
            let page_size: i64 = transaction.query_row("PRAGMA page_size", [], |row| row.get(0))?;
            if super::MAX_DATABASE_BYTES - page_count * page_size
                < 4 * 1024 * 1024 + footprint(MAX_EVENTS, page_size)
            {
                return Err(StoreError::StorageFull);
            }
            append(
                &transaction,
                id,
                CheckEvent::ProviderCheckStarted {
                    start: start.clone(),
                },
            )?;
            match start.target {
                CheckTarget::Custom { name } => evidence::clear(&transaction, &name)?,
                CheckTarget::Native { .. } => {
                    transaction.execute("DELETE FROM native_provider_evidence WHERE check_id IN (SELECT check_id FROM provider_check_events WHERE target_key=?1)", [start.target.key()?.as_slice()])?;
                }
                CheckTarget::Chatgpt { .. } => {
                    transaction.commit()?;
                    return load(connection, id);
                }
            }
            Some(append(
                &transaction,
                id,
                CheckEvent::ProviderCheckEvidenceCleared,
            )?)
        }
        CheckOp::Append { id, event } => {
            if matches!(
                event,
                CheckEvent::ProviderCheckStarted { .. }
                    | CheckEvent::ProviderCheckFinished {
                        disposition: CheckDisposition::Succeeded,
                        ..
                    }
            ) {
                return Err(StoreError::InvalidHistory);
            }
            Some(append(&transaction, id, event)?)
        }
        CheckOp::PublishProvider { id, record } => {
            let view = load(&transaction, id)?.ok_or(StoreError::InvalidHistory)?;
            if view.start.target
                != (CheckTarget::Custom {
                    name: record.name.clone(),
                })
                || view.start.fingerprint != record.digest
                || view.publication != Some(provider_digest(&record)?)
            {
                return Err(StoreError::InvalidHistory);
            }
            evidence::record_provider(&transaction, id, &record)?;
            Some(append(
                &transaction,
                id,
                CheckEvent::ProviderCheckFinished {
                    disposition: CheckDisposition::Succeeded,
                    evidence: view.publication,
                },
            )?)
        }
        CheckOp::PublishNative { id, record } => {
            let view = load(&transaction, id)?.ok_or(StoreError::InvalidHistory)?;
            if !matches!(view.start.target, CheckTarget::Native { .. })
                || view.start.fingerprint != record.fingerprint
                || view.publication != Some(native_digest(&record)?)
            {
                return Err(StoreError::InvalidHistory);
            }
            evidence::record_native(&transaction, id, &record)?;
            Some(append(
                &transaction,
                id,
                CheckEvent::ProviderCheckFinished {
                    disposition: CheckDisposition::Succeeded,
                    evidence: view.publication,
                },
            )?)
        }
    };
    #[cfg(all(test, target_os = "linux"))]
    super::journal::crash_tests::before_commit();
    transaction.commit()?;
    Ok(result)
}

fn footprint(events: usize, page_size: i64) -> i64 {
    events as i64 * (MAX_PAYLOAD as i64 + 16 * page_size)
}
pub(super) fn reserved_bytes(connection: &Connection, page_size: i64) -> Result<i64, StoreError> {
    let latest: Option<String> = connection
        .query_row(
            "SELECT check_id FROM provider_check_events ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let view = latest
        .map(|id| load(connection, id.parse()?))
        .transpose()?
        .flatten();
    Ok(view
        .filter(|view| view.disposition.is_none())
        .map_or(0, |view| footprint(MAX_EVENTS - view.events, page_size)))
}
pub(crate) fn provider_digest(record: &ProviderEvidenceRecord) -> Result<[u8; 32], StoreError> {
    Ok(Sha256::digest(serde_json::to_vec(record).map_err(|_| StoreError::InvalidHistory)?).into())
}
pub(crate) fn native_digest(record: &NativeEvidenceRecord) -> Result<[u8; 32], StoreError> {
    Ok(Sha256::digest(serde_json::to_vec(record).map_err(|_| StoreError::InvalidHistory)?).into())
}

fn append(
    connection: &Connection,
    id: CheckId,
    event: CheckEvent,
) -> Result<CheckView, StoreError> {
    let previous = load(connection, id)?;
    let view = CheckReducer::apply(id, previous, &event)?;
    let payload = serde_json::to_string(&event).map_err(|_| StoreError::InvalidHistory)?;
    if payload.len() > MAX_PAYLOAD {
        return Err(StoreError::InvalidHistory);
    }
    connection.execute("INSERT INTO provider_check_events (check_id, target_key, kind, event_version, payload, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![id.to_string(), view.start.target.key()?.as_slice(), event.kind(), event.version(), payload, now_ms()?])?;
    Ok(view)
}
fn decode(row: &rusqlite::Row<'_>) -> Result<(CheckId, [u8; 32], CheckEvent), StoreError> {
    let (id_len, key_len, kind_len, payload_len): (i64, i64, i64, i64) =
        (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
    if id_len != 36
        || key_len != 32
        || !(1..=40).contains(&kind_len)
        || !(1..=MAX_PAYLOAD as i64).contains(&payload_len)
    {
        return Err(StoreError::InvalidHistory);
    }
    let id: String = row.get(4)?;
    let key: Vec<u8> = row.get(5)?;
    let kind: String = row.get(6)?;
    let version: i64 = row.get(7)?;
    let payload: String = row.get(8)?;
    let stamp: i64 = row.get(9)?;
    if stamp < 0
        || !CHECK_FORMATS
            .iter()
            .any(|(name, _, current)| *name == kind && *current == version)
    {
        return Err(StoreError::InvalidHistory);
    }
    let event: CheckEvent =
        serde_json::from_str(&payload).map_err(|_| StoreError::InvalidHistory)?;
    if event.kind() != kind
        || serde_json::to_string(&event).map_err(|_| StoreError::InvalidHistory)? != payload
    {
        return Err(StoreError::InvalidHistory);
    }
    Ok((
        id.parse()?,
        key.try_into().map_err(|_| StoreError::InvalidHistory)?,
        event,
    ))
}
const COLUMNS: &str = "length(CAST(check_id AS BLOB)), length(target_key), length(CAST(kind AS BLOB)), length(CAST(payload AS BLOB)), check_id, target_key, kind, event_version, payload, created_at_ms";
fn load(connection: &Connection, id: CheckId) -> Result<Option<CheckView>, StoreError> {
    let mut statement = connection.prepare(&format!(
        "SELECT {COLUMNS} FROM provider_check_events WHERE check_id=?1 ORDER BY sequence"
    ))?;
    let mut rows = statement.query([id.to_string()])?;
    let mut view = None;
    while let Some(row) = rows.next()? {
        let (observed, key, event) = decode(row)?;
        if observed != id {
            return Err(StoreError::InvalidHistory);
        }
        let next = CheckReducer::apply(id, view, &event)?;
        if next.start.target.key()? != key {
            return Err(StoreError::InvalidHistory);
        }
        view = Some(next);
    }
    Ok(view)
}
fn views(connection: &Connection) -> Result<HashMap<CheckId, CheckView>, StoreError> {
    let mut statement = connection.prepare(&format!(
        "SELECT {COLUMNS}, sequence FROM provider_check_events ORDER BY sequence"
    ))?;
    let mut rows = statement.query([])?;
    let mut views = HashMap::new();
    let mut sequence = 0_i64;
    let mut count = 0;
    let mut active = None;
    while let Some(row) = rows.next()? {
        count += 1;
        let next = row.get::<_, i64>(10)?;
        if next <= sequence || count > MAX_CHECKS * MAX_EVENTS {
            return Err(StoreError::InvalidHistory);
        }
        sequence = next;
        let (id, key, event) = decode(row)?;
        if active.is_some_and(|current| current != id) {
            return Err(StoreError::InvalidHistory);
        }
        let view = CheckReducer::apply(id, views.remove(&id), &event)?;
        active = view.disposition.is_none().then_some(id);
        if view.start.target.key()? != key {
            return Err(StoreError::InvalidHistory);
        }
        views.insert(id, view);
        if views.len() > MAX_CHECKS {
            return Err(StoreError::InvalidHistory);
        }
    }
    Ok(views)
}
pub(super) fn validate(connection: &Connection) -> Result<(), StoreError> {
    views(connection).map(|_| ())
}
pub(super) fn evidence_valid(
    connection: &Connection,
    id: &str,
    fingerprint: [u8; 32],
    digest: [u8; 32],
) -> Result<bool, StoreError> {
    let Some(view) = load(connection, id.parse()?)? else {
        return Ok(false);
    };
    if view.disposition != Some(CheckDisposition::Succeeded)
        || view.start.fingerprint != fingerprint
        || view.publication != Some(digest)
    {
        return Ok(false);
    }
    let latest: Option<String> = connection.query_row("SELECT check_id FROM provider_check_events WHERE target_key=?1 ORDER BY sequence DESC LIMIT 1", [view.start.target.key()?.as_slice()], |row| row.get(0)).optional()?;
    Ok(latest.as_deref() == Some(id))
}

#[cfg(test)]
fn seed(
    connection: &Connection,
    record: crate::provider::evidence::EvidenceRecord,
) -> Result<CheckView, StoreError> {
    use crate::provider::evidence::EvidenceRecord;
    use crate::{CheckCallKind, CheckProbe, CheckStart, CheckUsage, Effort};
    let id = CheckId::new();
    let (target, fingerprint, digest, pages) = match &record {
        EvidenceRecord::Provider(record) => (
            CheckTarget::Custom {
                name: record.name.clone(),
            },
            record.digest,
            provider_digest(record)?,
            0,
        ),
        EvidenceRecord::Native(record) => (
            CheckTarget::Native {
                profile: "openai".into(),
                account: None,
                model: format!(
                    "fixture-{}",
                    record
                        .fingerprint
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                ),
                effort: Effort::High,
                workspace: None,
            },
            record.fingerprint,
            native_digest(record)?,
            1,
        ),
    };
    let probes: Vec<_> = [
        CheckCallKind::Answer,
        CheckCallKind::Delegate,
        CheckCallKind::Compaction,
    ]
    .into_iter()
    .map(|kind| CheckProbe {
        kind,
        effort: (pages > 0).then_some(Effort::High),
        output_tokens: if pages > 0 {
            1024
        } else if kind == CheckCallKind::Answer {
            4096
        } else {
            128
        },
    })
    .collect();
    let model = match &target {
        CheckTarget::Native { model, .. } => model.clone(),
        _ => "model-1".into(),
    };
    let start = CheckStart::new(target, model, fingerprint, [1; 32], pages, probes.clone())?;
    append(connection, id, CheckEvent::ProviderCheckStarted { start })?;
    let mut view = append(connection, id, CheckEvent::ProviderCheckEvidenceCleared)?;
    for kind in (pages > 0)
        .then_some(CheckCallKind::Catalog)
        .into_iter()
        .chain(probes.iter().map(|p| p.kind))
    {
        let ordinal = view.completed_calls;
        append(
            connection,
            id,
            CheckEvent::ProviderCheckCallStarted {
                ordinal,
                kind,
                destination: [1; 32],
                calls_remaining: view.calls_remaining(),
                output_remaining: view.output_remaining(),
                time_remaining_ms: 1000,
            },
        )?;
        view = append(
            connection,
            id,
            CheckEvent::ProviderCheckCallFinished {
                ordinal,
                disposition: CheckDisposition::Succeeded,
                usage: (kind != CheckCallKind::Catalog).then_some(CheckUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
                catalog_complete: kind == CheckCallKind::Catalog,
            },
        )?;
    }
    append(
        connection,
        id,
        CheckEvent::ProviderCheckEvidenceStarted {
            digest,
            snapshot: fingerprint,
        },
    )?;
    match record {
        EvidenceRecord::Provider(record) => evidence::record_provider(connection, id, &record)?,
        EvidenceRecord::Native(record) => evidence::record_native(connection, id, &record)?,
    }
    append(
        connection,
        id,
        CheckEvent::ProviderCheckFinished {
            disposition: CheckDisposition::Succeeded,
            evidence: Some(digest),
        },
    )
}
