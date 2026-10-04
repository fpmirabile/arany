use super::{NativeEvidenceRecord, ProviderEvidenceRecord, StoreError};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    net::SocketAddr,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_EVIDENCE_ROWS: i64 = 32;
const MAX_NATIVE_EVIDENCE_ROWS: i64 = 256;
const MAX_ADDRESSES: usize = 8;
const MAX_EVIDENCE_AGE_MS: i64 = 24 * 60 * 60 * 1000;

pub(super) fn clear(connection: &mut Connection, name: &str) -> Result<(), StoreError> {
    if !valid_name(name) {
        return Err(StoreError::InvalidHistory);
    }
    connection.execute(
        "DELETE FROM provider_evidence WHERE profile_name = ?1",
        [name],
    )?;
    Ok(())
}

pub(super) fn record(
    connection: &mut Connection,
    record: &ProviderEvidenceRecord,
) -> Result<(), StoreError> {
    validate(record)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "DELETE FROM provider_evidence WHERE expires_at_ms <= ?1",
        [record.checked_at_ms],
    )?;
    let count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM provider_evidence WHERE profile_name <> ?1",
        [&record.name],
        |row| row.get(0),
    )?;
    if count >= MAX_EVIDENCE_ROWS {
        return Err(StoreError::StorageFull);
    }
    let addresses = serde_json::to_string(
        &record
            .addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    )
    .map_err(|_| StoreError::InvalidHistory)?;
    transaction.execute(
        "INSERT INTO provider_evidence (profile_name, profile_digest, addresses, checked_at_ms, expires_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(profile_name) DO UPDATE SET
             profile_digest = excluded.profile_digest,
             addresses = excluded.addresses,
             checked_at_ms = excluded.checked_at_ms,
             expires_at_ms = excluded.expires_at_ms",
        params![
            record.name,
            record.digest.as_slice(),
            addresses,
            record.checked_at_ms,
            record.expires_at_ms
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn load(
    connection: &Connection,
    name: &str,
) -> Result<Option<ProviderEvidenceRecord>, StoreError> {
    if !valid_name(name) {
        return Err(StoreError::InvalidHistory);
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 1 {
        return Ok(None);
    }
    let raw: Option<(Vec<u8>, String, i64, i64)> = connection
        .query_row(
            "SELECT profile_digest, addresses, checked_at_ms, expires_at_ms
             FROM provider_evidence WHERE profile_name = ?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((digest, addresses, checked_at_ms, expires_at_ms)) = raw else {
        return Ok(None);
    };
    let digest: [u8; 32] = digest.try_into().map_err(|_| StoreError::InvalidHistory)?;
    if addresses.len() > 512 {
        return Err(StoreError::InvalidHistory);
    }
    let addresses: Vec<String> =
        serde_json::from_str(&addresses).map_err(|_| StoreError::InvalidHistory)?;
    let addresses = addresses
        .iter()
        .map(|address| SocketAddr::from_str(address).map_err(|_| StoreError::InvalidHistory))
        .collect::<Result<Vec<_>, _>>()?;
    let record = ProviderEvidenceRecord {
        name: name.to_owned(),
        digest,
        addresses,
        checked_at_ms,
        expires_at_ms,
    };
    validate(&record)?;
    let now_ms = now_ms()?;
    if record.checked_at_ms > now_ms || record.expires_at_ms <= now_ms {
        return Ok(None);
    }
    Ok(Some(record))
}

pub(super) fn clear_native(
    connection: &mut Connection,
    fingerprint: &[u8; 32],
) -> Result<(), StoreError> {
    connection.execute(
        "DELETE FROM native_provider_evidence WHERE fingerprint = ?1",
        [fingerprint.as_slice()],
    )?;
    Ok(())
}

pub(super) fn record_native(
    connection: &mut Connection,
    record: &NativeEvidenceRecord,
) -> Result<(), StoreError> {
    validate_native(record)?;
    let now = now_ms()?;
    if record.checked_at_ms > now || record.expires_at_ms <= now {
        return Err(StoreError::InvalidHistory);
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "DELETE FROM native_provider_evidence WHERE expires_at_ms <= ?1",
        [now],
    )?;
    let count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM native_provider_evidence WHERE fingerprint <> ?1",
        [record.fingerprint.as_slice()],
        |row| row.get(0),
    )?;
    if count >= MAX_NATIVE_EVIDENCE_ROWS {
        return Err(StoreError::StorageFull);
    }
    transaction.execute(
        "INSERT INTO native_provider_evidence (fingerprint, checked_at_ms, expires_at_ms)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(fingerprint) DO UPDATE SET
             checked_at_ms = excluded.checked_at_ms,
             expires_at_ms = excluded.expires_at_ms",
        params![
            record.fingerprint.as_slice(),
            record.checked_at_ms,
            record.expires_at_ms
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn load_native(
    connection: &Connection,
    fingerprint: &[u8; 32],
) -> Result<Option<NativeEvidenceRecord>, StoreError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 3 {
        return Ok(None);
    }
    let raw: Option<(i64, i64)> = connection
        .query_row(
            "SELECT checked_at_ms, expires_at_ms
             FROM native_provider_evidence WHERE fingerprint = ?1",
            [fingerprint.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((checked_at_ms, expires_at_ms)) = raw else {
        return Ok(None);
    };
    let record = NativeEvidenceRecord {
        fingerprint: *fingerprint,
        checked_at_ms,
        expires_at_ms,
    };
    validate_native(&record)?;
    let now = now_ms()?;
    if record.checked_at_ms > now || record.expires_at_ms <= now {
        return Ok(None);
    }
    Ok(Some(record))
}

fn validate_native(record: &NativeEvidenceRecord) -> Result<(), StoreError> {
    if record.checked_at_ms < 0
        || record.expires_at_ms <= record.checked_at_ms
        || record.expires_at_ms - record.checked_at_ms > MAX_EVIDENCE_AGE_MS
    {
        return Err(StoreError::InvalidHistory);
    }
    Ok(())
}

fn now_ms() -> Result<i64, StoreError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::InvalidHistory)?
        .as_millis();
    i64::try_from(now).map_err(|_| StoreError::InvalidHistory)
}

fn validate(record: &ProviderEvidenceRecord) -> Result<(), StoreError> {
    if !valid_name(&record.name)
        || record.addresses.is_empty()
        || record.addresses.len() > MAX_ADDRESSES
        || record.addresses.windows(2).any(|pair| pair[0] >= pair[1])
        || record.checked_at_ms < 0
        || record.expires_at_ms <= record.checked_at_ms
        || record.expires_at_ms - record.checked_at_ms > MAX_EVIDENCE_AGE_MS
    {
        return Err(StoreError::InvalidHistory);
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{StateRoot, Store};

    #[tokio::test]
    async fn native_evidence_survives_reopen_only_for_the_exact_fingerprint() {
        let temp = tempfile::tempdir().expect("private root");
        let path = temp.path().join("state");
        let store = Store::open(StateRoot::admit(&path).expect("state root")).expect("Store");
        let now = now_ms().expect("clock");
        let record = NativeEvidenceRecord {
            fingerprint: [1; 32],
            checked_at_ms: now,
            expires_at_ms: now + 60_000,
        };
        store
            .record_native_evidence(record.clone())
            .await
            .expect("record evidence");
        store.close().await.expect("close Store");

        let store = Store::open_read_only(StateRoot::open_existing(&path).expect("state root"))
            .expect("read-only Store");
        assert_eq!(
            store
                .load_native_evidence(record.fingerprint)
                .await
                .unwrap(),
            Some(record.clone())
        );
        assert_eq!(store.load_native_evidence([2; 32]).await.unwrap(), None);
        assert!(matches!(
            store.clear_native_evidence(record.fingerprint).await,
            Err(StoreError::ReadOnly)
        ));
        store.close().await.expect("close read-only Store");

        let store = Store::open(StateRoot::open_existing(&path).expect("state root"))
            .expect("writable Store");
        store
            .clear_native_evidence(record.fingerprint)
            .await
            .expect("clear evidence");
        assert_eq!(
            store
                .load_native_evidence(record.fingerprint)
                .await
                .unwrap(),
            None
        );
        store.close().await.expect("close Store");
    }

    #[tokio::test]
    async fn native_evidence_rejects_bad_age_and_malformed_stored_rows() {
        let temp = tempfile::tempdir().expect("private root");
        let path = temp.path().join("state");
        let store = Store::open(StateRoot::admit(&path).expect("state root")).expect("Store");
        let now = now_ms().expect("clock");
        let record = NativeEvidenceRecord {
            fingerprint: [3; 32],
            checked_at_ms: now,
            expires_at_ms: now + 60_000,
        };
        let invalid = NativeEvidenceRecord {
            expires_at_ms: now + MAX_EVIDENCE_AGE_MS + 1,
            ..record.clone()
        };
        assert!(matches!(
            store.record_native_evidence(invalid).await,
            Err(StoreError::InvalidHistory)
        ));
        store
            .record_native_evidence(record.clone())
            .await
            .expect("record evidence");
        store.close().await.expect("close Store");

        let connection = Connection::open(path.join("events.sqlite3")).expect("database");
        connection
            .execute(
                "UPDATE native_provider_evidence
                 SET checked_at_ms = ?1, expires_at_ms = ?2 WHERE fingerprint = ?3",
                params![now - 60_000, now - 1, record.fingerprint.as_slice()],
            )
            .expect("expire derived evidence");
        drop(connection);
        let store = Store::open_read_only(StateRoot::open_existing(&path).expect("state root"))
            .expect("read-only Store");
        assert_eq!(
            store
                .load_native_evidence(record.fingerprint)
                .await
                .unwrap(),
            None
        );
        store.close().await.expect("close read-only Store");

        let connection = Connection::open(path.join("events.sqlite3")).expect("database");
        connection
            .execute(
                "UPDATE native_provider_evidence SET expires_at_ms = ?1 WHERE fingerprint = ?2",
                params![now + MAX_EVIDENCE_AGE_MS + 1, record.fingerprint.as_slice()],
            )
            .expect("corrupt derived evidence");
        drop(connection);

        let store = Store::open_read_only(StateRoot::open_existing(&path).expect("state root"))
            .expect("read-only Store");
        assert!(matches!(
            store.load_native_evidence(record.fingerprint).await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("close read-only Store");
    }

    #[tokio::test]
    async fn native_evidence_table_has_a_finite_row_limit() {
        let temp = tempfile::tempdir().expect("private root");
        let path = temp.path().join("state");
        let store = Store::open(StateRoot::admit(&path).expect("state root")).expect("Store");
        let now = now_ms().expect("clock");
        for value in 0..MAX_NATIVE_EVIDENCE_ROWS {
            let mut fingerprint = [0; 32];
            fingerprint[0] = u8::try_from(value).expect("bounded fingerprint");
            store
                .record_native_evidence(NativeEvidenceRecord {
                    fingerprint,
                    checked_at_ms: now,
                    expires_at_ms: now + 60_000,
                })
                .await
                .expect("bounded evidence row");
        }
        let extra = NativeEvidenceRecord {
            fingerprint: [9; 32],
            checked_at_ms: now,
            expires_at_ms: now + 60_000,
        };
        assert!(matches!(
            store.record_native_evidence(extra.clone()).await,
            Err(StoreError::StorageFull)
        ));
        store.clear_native_evidence([0; 32]).await.unwrap();
        store.record_native_evidence(extra).await.unwrap();
        store.close().await.expect("close Store");
    }
}
