use super::super::replay::{
    BEFORE_DECODE, HEAD_FIRST_SQL, HEAD_TAIL_SQL, LANE_SQL, LINEAGE_COUNT_SQL, NEXT_HEAD_SQL,
    WATERMARK_SQL, list_all_sessions, list_page_in_snapshot, list_session_page,
};
use super::*;
use crate::session::WorkspaceIdentity;

#[test]
fn append_refuses_event_past_replay_limit() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let mut hot_session = None;
    let session_id = SessionId::new();
    append(
        &mut connection,
        &mut hot_session,
        session_id,
        Event::SessionStarted {
            title: "Session".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 1,
            },
        },
    )
    .expect("first Event");

    let rename = Event::SessionRenamed { title: "x".into() };
    let payload = rename.payload().expect("bounded Event payload");
    let transaction = connection.transaction().expect("seed transaction");
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO events (sequence, session_id, kind, payload, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, 0)",
            )
            .expect("seed statement");
        for sequence in 2..=MAX_SESSION_EVENTS {
            insert
                .execute(params![
                    sequence as i64,
                    session_id.to_string(),
                    rename.kind(),
                    payload,
                ])
                .expect("valid Event row");
        }
    }
    transaction.commit().expect("seeded prefix");

    assert!(matches!(
        append(&mut connection, &mut hot_session, session_id, rename),
        Err(StoreError::ReplayLimit)
    ));
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("durable Event count");
    assert_eq!(count, MAX_SESSION_EVENTS as i64);
}

async fn setup_store_with_remaining_slots(
    root: &StateRoot,
    remaining: usize,
) -> (crate::Store, SessionId) {
    let store = crate::Store::open(root.try_clone().unwrap()).unwrap();
    let session = SessionId::new();
    store
        .append(
            session,
            Event::SessionStarted {
                title: "Capacity".into(),
                workspace_identity: WorkspaceIdentity {
                    device: 1,
                    inode: 1,
                },
            },
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut connection = open_connection(root, false).unwrap();
    let rename = Event::SessionRenamed {
        title: "Capacity".into(),
    };
    let payload = rename.payload().unwrap();
    let transaction = connection.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO events (sequence, session_id, kind, payload, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, 0)",
            )
            .unwrap();
        for sequence in 2..=MAX_SESSION_EVENTS - remaining {
            insert
                .execute(params![
                    sequence as i64,
                    session.to_string(),
                    rename.kind(),
                    payload
                ])
                .unwrap();
        }
    }
    transaction.commit().unwrap();
    drop(connection);
    (
        crate::Store::open(root.try_clone().unwrap()).unwrap(),
        session,
    )
}

#[tokio::test]
async fn store_reopens_maximum_worst_case_escaped_compaction_summary() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let fixture = crate::session::worker_compacted_fixture(true);
    let compacted_index = fixture
        .events
        .iter()
        .position(|event| matches!(event, Event::GoalContextCompacted { .. }))
        .expect("compaction fixture Event");
    let store = crate::Store::open(root.try_clone().expect("State handle")).expect("Store");
    for event in &fixture.events[..compacted_index] {
        store
            .append(fixture.session, event.clone())
            .await
            .expect("prefix Event");
    }
    let prefix = store
        .load_session(fixture.session)
        .await
        .expect("committed prefix");
    let Event::GoalContextCompacted {
        goal_id,
        run_id,
        covered_tool_count,
        record,
    } = fixture.events[compacted_index].clone()
    else {
        unreachable!()
    };
    let summary = "\u{1}".repeat(crate::session::MAX_COMPACTION_SUMMARY_BYTES);
    let mut record = successful_compaction_record(record, &summary);
    record.source_digest = crate::session::prefix_digest(&prefix, record.covered_sequence)
        .expect("stored prefix digest");
    let session_record = record.clone();
    let event = Event::GoalContextCompacted {
        goal_id,
        run_id,
        covered_tool_count,
        record,
    };
    let payload = event.payload().expect("valid compaction Event");
    assert!(payload.len() > super::super::MAX_EVENT_BYTES);
    assert!(payload.len() <= super::super::MAX_COMPACTION_EVENT_BYTES);
    store
        .admit(
            fixture.session,
            1,
            0,
            crate::store::AdmissionKind::Compaction,
        )
        .await
        .expect("compaction admission headroom");
    store
        .append(fixture.session, event)
        .await
        .expect("bounded compaction Event");
    for event in &fixture.events[compacted_index + 1..] {
        store
            .append(fixture.session, event.clone())
            .await
            .expect("remaining Event");
    }
    let view = store
        .load_view(fixture.session)
        .await
        .expect("Session view")
        .expect("Session");
    let run = &view.runs[0];
    let compacted_prefix = store
        .load_session(fixture.session)
        .await
        .expect("stored compaction prefix");
    let mut record = session_record;
    record.covered_sequence = run.finished_sequence.expect("finished Run");
    record.source_digest =
        crate::session::prefix_digest(&compacted_prefix, record.covered_sequence)
            .expect("completed Run digest");
    let event = Event::ContextCompacted {
        record,
        active_ms: Some(0),
    };
    let payload = event.payload().expect("valid Session compaction Event");
    assert!(payload.len() > super::super::MAX_EVENT_BYTES);
    assert!(payload.len() <= super::super::MAX_COMPACTION_EVENT_BYTES);
    store
        .admit(
            fixture.session,
            2,
            0,
            crate::store::AdmissionKind::Compaction,
        )
        .await
        .expect("Session compaction admission headroom");
    store
        .append(
            fixture.session,
            Event::GoalCompactionCallReserved {
                goal_id,
                run_id,
                active_ceiling_ms: 0,
            },
        )
        .await
        .expect("Session compaction call reservation");
    store
        .append(fixture.session, event)
        .await
        .expect("bounded Session compaction Event");
    store.close().await.expect("close Store");

    let reopened = crate::Store::open(root).expect("reopened Store");
    let view = reopened
        .load_view(fixture.session)
        .await
        .expect("replayed Session")
        .expect("Session");
    assert_eq!(
        view.runs[0].tool_compaction_summary.as_deref(),
        Some(summary.as_str())
    );
    assert!(matches!(
        &view.compactions[0].record.status,
        CompactionStatus::Succeeded { summary: stored, .. } if stored == &summary
    ));
    assert_eq!(
        view.goal(goal_id)
            .expect("replayed Goal")
            .compaction_call_reserved,
        None
    );
    reopened.close().await.expect("close reopened Store");
}

fn successful_compaction_record(
    mut record: crate::session::CompactionRecord,
    summary: &str,
) -> crate::session::CompactionRecord {
    let summary_bytes = summary.len() as u32;
    record.status = crate::session::CompactionStatus::Succeeded {
        content_digest: crate::validate::digest(summary.as_bytes()),
        summary_bytes,
        summary_tokens_estimate: summary_bytes.div_ceil(4),
        summary: summary.to_owned(),
    };
    record
}

async fn publish_setup_profile(
    store: &crate::Store,
    root: &StateRoot,
    session: SessionId,
    draft: crate::CustomProfileDraft,
) -> Result<EventEnvelope, StoreError> {
    use crate::{ProviderSetupDisposition, ProviderSetupTarget, ProviderSetupTargetKind};
    use sha2::{Digest, Sha256};
    let operation_id = uuid::Uuid::now_v7();
    store
        .append(
            session,
            Event::ProviderSetupEditStarted {
                operation_id,
                target: ProviderSetupTarget {
                    kind: ProviderSetupTargetKind::CustomProfile,
                    identity: draft.name.clone(),
                },
                expected_digest: None,
            },
        )
        .await?;
    let bytes = crate::CustomProfile::write_profiles(root, None, &[draft]).unwrap();
    store
        .append(
            session,
            Event::ProviderSetupEditFinished {
                operation_id,
                resulting_digest: Some(Sha256::digest(bytes).into()),
                disposition: ProviderSetupDisposition::Applied,
            },
        )
        .await
}

#[tokio::test]
async fn provider_setup_admission_refuses_publication_when_only_one_event_fits() {
    use crate::{CustomProfile, CustomProfileDraft, CustomProtocol, MessagesAuth, RecordKind};

    for remaining in 1..=super::super::PROVIDER_SETUP_EDIT_EVENT_SLOTS {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let (store, session) = setup_store_with_remaining_slots(&root, remaining).await;
        let draft = CustomProfileDraft {
            name: "example-route".into(),
            protocol: CustomProtocol::OpenaiResponses,
            endpoint: "https://api.example.com/v1/responses".into(),
            model: "model-synthetic".into(),
            credential_env: "ARANY_PROVIDER_EXAMPLE_KEY".into(),
            messages_auth: MessagesAuth::Bearer,
            efforts: Vec::new(),
        };
        CustomProfile::validate_drafts(std::slice::from_ref(&draft)).unwrap();
        let result = publish_setup_profile(&store, &root, session, draft).await;
        let applied = remaining == super::super::PROVIDER_SETUP_EDIT_EVENT_SLOTS;
        if applied {
            result.unwrap();
        } else {
            assert!(matches!(result, Err(StoreError::ReplayLimit)));
        }
        store.close().await.unwrap();
        assert_eq!(
            root.record(RecordKind::ProviderProfiles)
                .read()
                .unwrap()
                .is_some(),
            applied
        );
        let reopened = crate::Store::open_read_only(root.try_clone().unwrap()).unwrap();
        let events = reopened.load_session(session).await.unwrap();
        assert_eq!(
            events.len(),
            MAX_SESSION_EVENTS - if applied { 0 } else { remaining }
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::ProviderSetupEditStarted { .. }))
                .count(),
            usize::from(applied)
        );
        let view = reopened.load_view(session).await.unwrap().unwrap();
        assert_eq!(view.pending_provider_setup_edit, None);
        assert!(!view.provider_setup_uncertain);
        reopened.close().await.unwrap();
    }
}

fn hot_history(path: &std::path::Path) -> (Connection, Option<HotSession>, SessionId) {
    let mut connection = Connection::open(path).expect("file-backed store");
    connection
        .execute_batch(SCHEMA_SQL)
        .expect("journal schema");
    let mut hot = None;
    let session = SessionId::new();
    append(
        &mut connection,
        &mut hot,
        session,
        Event::SessionStarted {
            title: "Session".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 1,
            },
        },
    )
    .expect("first Event");
    assert!(hot.is_some());
    (connection, hot, session)
}

fn admit_one(
    connection: &mut Connection,
    cached: Option<&HotSession>,
    session_id: SessionId,
) -> Result<(), StoreError> {
    admit(
        connection,
        cached,
        Admission {
            session_id,
            event_slots: 1,
            tool_observations: 0,
            kind: AdmissionKind::Operation,
        },
    )
}

fn write_racing_commit<T>(
    connection: &mut Connection,
    second: Connection,
    write: impl FnOnce(&mut Connection) -> Result<T, StoreError>,
) -> Connection {
    thread_local! {
        static WRITER: std::cell::RefCell<Option<Connection>> = const { std::cell::RefCell::new(None) };
    }
    second
        .execute_batch(
            "BEGIN IMMEDIATE; UPDATE events SET created_at_ms=created_at_ms+1 WHERE sequence=2;",
        )
        .expect("pending other writer");
    WRITER.with(|writer| *writer.borrow_mut() = Some(second));
    connection
        .busy_handler(Some(|attempt| {
            attempt == 0
                && WRITER.with(|writer| {
                    writer
                        .borrow()
                        .as_ref()
                        .expect("pending other writer")
                        .execute_batch("COMMIT")
                        .is_ok()
                })
        }))
        .expect("commit other writer when BEGIN IMMEDIATE blocks");
    let refused = write(connection);
    connection.busy_handler(None).expect("remove test handler");
    let second = WRITER.with(|writer| writer.borrow_mut().take()).unwrap();
    assert!(
        matches!(&refused, Err(StoreError::HistoryChanged)),
        "expected HistoryChanged, got {:?}",
        refused.err()
    );
    assert!(connection.is_autocommit());
    second
}

#[test]
fn other_connection_changes_invalidate_hot_history() {
    let temp = tempfile::tempdir().expect("private test root");
    let path = temp.path().join(DATABASE_FILE);
    let (mut connection, mut hot, session) = hot_history(&path);
    admit_one(&mut connection, hot.as_ref(), session).expect("cached admission");
    let mut second = Connection::open(&path).expect("second connection");
    seed_session(
        &mut second,
        &mut None,
        WorkspaceIdentity {
            device: 1,
            inode: 1,
        },
        0,
    );
    admit_one(&mut connection, hot.as_ref(), session).expect("admit after version drift");
    let second = write_racing_commit(&mut connection, second, |connection| {
        append(
            connection,
            &mut hot,
            session,
            Event::SessionRenamed {
                title: "Racing".into(),
            },
        )
    });
    assert!(hot.is_none());
    let committed = append(
        &mut connection,
        &mut hot,
        session,
        Event::SessionRenamed {
            title: "Updated".into(),
        },
    )
    .expect("append after another writer");
    assert_eq!(committed.sequence, 3);
    assert!(hot.is_some());
    let second = write_racing_commit(&mut connection, second, |connection| {
        admit_one(connection, hot.as_ref(), session)
    });
    recover_after_unrelated_corruption(&mut connection, &mut hot, session, second);
}

fn recover_after_unrelated_corruption(
    connection: &mut Connection,
    hot: &mut Option<HotSession>,
    session: SessionId,
    second: Connection,
) {
    second
        .execute("UPDATE events SET kind='UnknownKind' WHERE sequence=2", [])
        .expect("corrupt unrelated committed Event");
    assert!(matches!(
        admit_one(connection, hot.as_ref(), session),
        Err(StoreError::InvalidHistory)
    ));
    assert!(matches!(
        append(
            connection,
            hot,
            session,
            Event::SessionRenamed {
                title: "Rejected".into()
            }
        ),
        Err(StoreError::InvalidHistory)
    ));
    assert!(hot.is_none());
    second
        .execute(
            "UPDATE events SET kind='SessionStarted' WHERE sequence=2",
            [],
        )
        .expect("restore test-owned Event");
    let recovered = append(
        connection,
        hot,
        session,
        Event::SessionRenamed {
            title: "Recovered".into(),
        },
    )
    .expect("full replay after invalidation");
    assert_eq!(recovered.sequence, 4);
}

#[test]
fn cached_admission_defers_unrelated_corruption_until_invalidation() {
    let temp = tempfile::tempdir().expect("private test root");
    let (mut connection, mut hot, session) = hot_history(&temp.path().join(DATABASE_FILE));
    seed_session(
        &mut connection,
        &mut None,
        WorkspaceIdentity {
            device: 1,
            inode: 1,
        },
        0,
    );
    append(
        &mut connection,
        &mut hot,
        session,
        Event::SessionRenamed {
            title: "Updated".into(),
        },
    )
    .expect("cache current global sequence");
    connection
        .execute("UPDATE events SET kind='UnknownKind' WHERE sequence=2", [])
        .expect("corrupt unrelated Event without changing version or sequence");
    admit_one(&mut connection, hot.as_ref(), session).expect("unchanged cache skips global scan");
    assert!(matches!(
        admit_one(&mut connection, None, session),
        Err(StoreError::InvalidHistory)
    ));
    let cache = hot.as_mut().unwrap();
    cache.data_version -= 1;
    assert!(matches!(
        admit_one(&mut connection, hot.as_ref(), session),
        Err(StoreError::InvalidHistory)
    ));
    let cache = hot.as_mut().unwrap();
    cache.data_version += 1;
    connection
        .execute("DELETE FROM events WHERE sequence=3", [])
        .expect("global sequence drift");
    assert!(matches!(
        admit_one(&mut connection, hot.as_ref(), session),
        Err(StoreError::InvalidHistory)
    ));
    connection
        .execute(
            "UPDATE events SET kind='SessionStarted' WHERE sequence=2",
            [],
        )
        .expect("restore unrelated Event");
    admit_one(&mut connection, None, session).expect("cold validation after repair");
}

#[test]
fn cached_forks_defer_unrelated_corruption_but_validate_the_source() {
    let temp = tempfile::tempdir().expect("private test root");
    let mut connection = Connection::open(temp.path().join(DATABASE_FILE)).unwrap();
    connection.execute_batch(SCHEMA_SQL).unwrap();
    let mut hot = None;
    let fixture = crate::session::worker_protocol_fixture();
    for event in &fixture.events {
        append(&mut connection, &mut hot, fixture.session, event.clone()).unwrap();
    }
    let other = seed_session(
        &mut connection,
        &mut None,
        WorkspaceIdentity {
            device: 1,
            inode: 1,
        },
        0,
    );
    append(
        &mut connection,
        &mut hot,
        fixture.session,
        Event::SessionRenamed {
            title: "Source".into(),
        },
    )
    .unwrap();
    let boundary = prepare_fork(&mut connection, hot.as_ref(), fixture.session, None).unwrap();
    connection
        .execute(
            "UPDATE events SET kind='UnknownKind' WHERE session_id=?1",
            [other.to_string()],
        )
        .unwrap();
    assert_eq!(
        prepare_fork(&mut connection, hot.as_ref(), fixture.session, None).unwrap(),
        boundary
    );
    assert!(matches!(
        prepare_fork(&mut connection, None, fixture.session, None),
        Err(StoreError::InvalidHistory)
    ));
    assert!(matches!(
        fork_at(&mut connection, None, boundary, "Cold".into()),
        Err(StoreError::InvalidHistory)
    ));
    let mut forged = boundary;
    forged.prefix_digest[0] ^= 1;
    assert!(matches!(
        fork_at(&mut connection, hot.as_ref(), forged, "Forged".into()),
        Err(StoreError::InvalidHistory)
    ));
    let child = fork_at(&mut connection, hot.as_ref(), boundary, "Cached".into()).unwrap();
    assert!(matches!(
        resolve_view(&connection, child, 0, &mut 0),
        Err(StoreError::InvalidHistory)
    ));
    assert!(matches!(
        prepare_fork(&mut connection, hot.as_ref(), fixture.session, None),
        Err(StoreError::InvalidHistory)
    ));
    connection
        .execute(
            "UPDATE events SET kind='SessionStarted' WHERE session_id=?1",
            [other.to_string()],
        )
        .unwrap();
    let (_, view) = resolve_view(&connection, child, 0, &mut 0).unwrap();
    assert_eq!(view.unwrap().lineage, Some(boundary));
}

#[test]
fn failed_insert_drops_hot_projection_before_recovery() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let mut hot_session = None;
    let session_id = SessionId::new();
    append(
        &mut connection,
        &mut hot_session,
        session_id,
        Event::SessionStarted {
            title: "Session".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 1,
            },
        },
    )
    .expect("first Event");
    assert!(hot_session.is_some());
    connection
        .execute_batch(
            "CREATE TRIGGER reject_rename BEFORE INSERT ON events
             WHEN NEW.kind='SessionRenamed'
             BEGIN SELECT RAISE(FAIL, 'test insert fault'); END;",
        )
        .expect("test-owned insert fault");
    assert!(matches!(
        append(
            &mut connection,
            &mut hot_session,
            session_id,
            Event::SessionRenamed {
                title: "Not committed".into(),
            },
        ),
        Err(StoreError::Sqlite(_))
    ));
    assert!(hot_session.is_none());
    connection
        .execute_batch("DROP TRIGGER reject_rename")
        .expect("remove test fault");
    let recovered = append(
        &mut connection,
        &mut hot_session,
        session_id,
        Event::SessionRenamed {
            title: "Committed".into(),
        },
    )
    .expect("recovered append");
    assert_eq!(recovered.sequence, 2);
    assert!(hot_session.is_some());
}

#[test]
fn run_start_admission_reserves_the_payload_exceptions() {
    assert_eq!(
        crate::store::RUN_START_EXTRA_BYTES,
        event_payload_limit("MessageAccepted") + event_payload_limit("GoalStarted")
            - 2 * crate::store::MAX_EVENT_BYTES
    );
}

// The largest typed Write: a six-byte escape per content byte and a long escaped path.
fn largest_tool_started_payload() -> String {
    use crate::tools::permissions::{
        Decision, MAX_HOOKS_PER_OCCURRENCE, PermissionMode, PermissionSource, PermissionVerdict,
    };
    let intent = crate::tools::EffectIntent {
        host: None,
        wrap: None,
        rewrite: Vec::new(),
        host_command: None,
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: crate::session::RunId::new(),
        agent_run_id: crate::session::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: u64::MAX,
        workspace_inode: u64::MAX,
        call: crate::tools::ToolCall::Write {
            path: "\"".repeat(512),
            expected_digest: Some("0".repeat(64)),
            content: "\u{1}".repeat(crate::tools::types::MAX_EDIT_CONTENT_BYTES),
        },
        limits: crate::tools::ToolLimits::default(),
        use_count: 1,
    };
    assert!(intent.valid());
    Event::ToolStarted {
        verdict: Some(PermissionVerdict {
            host: None,
            intent_digest: intent.digest(),
            decision: Decision::Ask,
            mode: Some(PermissionMode::AcceptEdits),
            basis: PermissionSource::Hook,
            source: PermissionSource::UnavailableApproval,
            approved: false,
            hook_restriction: Some(Decision::Ask),
            hooks: u8::try_from(MAX_HOOKS_PER_OCCURRENCE).unwrap(),
            narrowed: false,
        }),
        run_id: intent.run_id,
        agent_run_id: intent.agent_run_id,
        intent: intent.into(),
        active_ceiling_ms: u64::MAX,
    }
    .payload()
    .unwrap()
}

#[test]
fn tool_admission_near_capacity_reserves_the_largest_intent_and_its_completion() {
    for prepared in [false, true] {
        assert_tool_tail_capacity(prepared);
    }
}

const CAPACITY_PADDING_DIVISOR: i64 = 2;

fn pad_capacity(transaction: &Transaction<'_>, rows: i64, page_size: i64) {
    transaction.execute(
        "WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n < ?1) INSERT INTO capacity_padding SELECT zeroblob(?2) FROM rows",
        params![rows, page_size / CAPACITY_PADDING_DIVISOR],
    ).unwrap();
}

fn fill_admitted_capacity(
    transaction: &Transaction<'_>,
    page_size: i64,
    admits: impl Fn(&Transaction<'_>) -> bool,
) {
    assert!(admits(transaction));
    let mut batch = (ADMISSION_HEADROOM_BYTES / page_size / CAPACITY_PADDING_DIVISOR).max(1);
    while batch > 0 {
        transaction.execute_batch("SAVEPOINT step").unwrap();
        pad_capacity(transaction, batch, page_size);
        if admits(transaction) {
            transaction.execute_batch("RELEASE step").unwrap();
        } else {
            transaction
                .execute_batch("ROLLBACK TO step; RELEASE step")
                .unwrap();
            batch /= CAPACITY_PADDING_DIVISOR;
        }
    }
}

fn assert_tool_tail_capacity(prepared: bool) {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let session_id = SessionId::new();
    let started = largest_tool_started_payload();
    let mut proposal: serde_json::Value = serde_json::from_str(&started).unwrap();
    proposal.as_object_mut().unwrap().remove("verdict");
    let proposal = proposal.to_string();
    let completion_observations = 1;
    let intent_records = 1 + usize::from(prepared);
    let tail_slots = completion_observations + intent_records;
    let finished = format!(
        "\"{}\"",
        "x".repeat(crate::store::MAX_TOOL_FINISHED_BYTES - 2)
    );
    let transaction = connection.transaction().unwrap();
    let pages = |t: &Transaction<'_>| -> i64 {
        t.query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap()
    };
    let page_size: i64 = transaction
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    let admits = |t: &Transaction<'_>, observations: usize| {
        admit_in_transaction(
            t,
            session_id,
            tail_slots,
            observations,
            AdmissionKind::Operation,
        )
        .is_ok()
    };
    let tail_commits = |t: &Transaction<'_>| {
        t.execute_batch("SAVEPOINT pair").unwrap();
        for (kind, payload) in std::iter::once(("ToolProposed", &proposal))
            .filter(|_| prepared)
            .chain([("ToolStarted", &started), ("ToolFinished", &finished)])
        {
            t.execute(
                "INSERT INTO events (session_id, run_id, agent_run_id, kind, payload, created_at_ms) VALUES (?1, 'run', 'agent', ?2, ?3, 0)",
                params![session_id.to_string(), kind, payload],
            )
            .unwrap();
        }
        let room = MAX_DATABASE_BYTES - pages(t) * page_size;
        t.execute_batch("ROLLBACK TO pair; RELEASE pair").unwrap();
        room >= ADMISSION_HEADROOM_BYTES
    };
    transaction
        .execute_batch("CREATE TABLE capacity_padding(value BLOB NOT NULL) STRICT;")
        .unwrap();
    fill_admitted_capacity(&transaction, page_size, |t| admits(t, tail_slots));
    assert!(tail_commits(&transaction));
    pad_capacity(&transaction, 1, page_size);
    assert!(
        !admits(&transaction, tail_slots),
        "the intent is refused before dispatch"
    );
    fill_admitted_capacity(&transaction, page_size, |t| {
        admits(t, completion_observations)
    });
    assert!(
        !tail_commits(&transaction),
        "omitting intent records from admission would strand the step"
    );
}

#[test]
fn every_event_slot_admits_a_current_tool_observation() {
    assert_eq!(event_payload_limit("SupervisorVerdictRecorded"), 2048);
    assert_eq!(
        event_payload_limit("ToolFinished"),
        crate::tools::types::MAX_TOOL_OBSERVATION_BYTES + 512
    );
    assert_eq!(
        event_payload_limit("SessionRenamed"),
        crate::store::MAX_EVENT_BYTES
    );
    let payload = largest_tool_started_payload();
    assert!(payload.len() > crate::store::MAX_EVENT_BYTES);
    assert!(payload.len() <= event_payload_limit("ToolStarted"));
    let host = crate::session::session_host_protocol_fixture();
    let host_start = host.events.iter().rev().find(|event| matches!(event,
        Event::ToolStarted { verdict: Some(verdict), .. }
            if verdict.host.as_ref().is_some_and(|approval| approval.source_intent.is_some())
    )).expect("host start with a session approval source");
    assert!(host_start.payload().unwrap().len() <= event_payload_limit("ToolStarted"));
    assert_eq!(
        event_payload_limit("ToolProposed"),
        event_payload_limit("ToolStarted")
    );
    for kind in ["ContextCompacted", "GoalContextCompacted"] {
        assert_eq!(
            event_payload_limit(kind),
            crate::store::MAX_COMPACTION_EVENT_BYTES
        );
    }
}

#[test]
fn worker_protocol_store_cold_prefixes_replay() {
    for f in [crate::session::worker_protocol_fixture()] {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).expect("private worker State root");
        let mut connection = open_connection(&root, false).unwrap();
        let mut hot = None;
        for (index, event) in f.events.iter().enumerate() {
            append(&mut connection, &mut hot, f.session, event.clone()).unwrap();
            let cold = open_connection(&root, true).unwrap();
            let (_, view) = resolve_view(&cold, f.session, 0, &mut 0).unwrap();
            let expected = SessionView::replay(f.session, &f.envelopes()[..=index]).unwrap();
            assert_eq!(view, expected, "cold prefix {index}");
        }
    }
}

#[test]
fn rewind_store_cold_prefixes_replay_the_outcome() {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&temp.path().join("state")).expect("private rewind State root");
    let f = crate::session::worker_protocol_fixture();
    let mut connection = open_connection(&root, false).unwrap();
    let mut hot = None;
    for event in &f.events {
        append(&mut connection, &mut hot, f.session, event.clone()).unwrap();
    }
    let (_, view) = resolve_view(&connection, f.session, 0, &mut 0).unwrap();
    let view = view.unwrap();
    let event = crate::session::rewind_started_fixture(&view, view.rewinds.checkpoints[0].clone());
    append(&mut connection, &mut hot, f.session, event).unwrap();
    let (_, view) = resolve_view(&connection, f.session, 0, &mut 0).unwrap();
    let view = view.unwrap();
    assert!(view.rewinds.operations[0].finished.is_none());
    assert_eq!(
        view.rewinds.operations[0].outcome(),
        crate::session::rewind::RewindOutcome::Uncertain
    );
    let event = crate::session::rewind_finished_fixture(
        &view,
        crate::session::rewind::RewindOutcome::Restored,
    );
    append(&mut connection, &mut hot, f.session, event).unwrap();
    drop(hot);
    let cold = open_connection(&root, true).unwrap();
    let (_, view) = resolve_view(&cold, f.session, 0, &mut 0).unwrap();
    assert_eq!(
        view.unwrap().rewinds.operations[0].outcome(),
        crate::session::rewind::RewindOutcome::Restored
    );
}

fn pending_publication_history(started: bool) -> (SessionId, Vec<Event>) {
    let mut fixture = crate::session::worker_protocol_fixture();
    let end = fixture
        .events
        .iter()
        .rposition(|event| matches!(event, Event::ProviderCallRecorded { .. }))
        .unwrap();
    fixture.events.truncate(end + 1);
    let Event::ProviderCallRecorded { record, .. } = &mut fixture.events[end] else {
        unreachable!()
    };
    record.disposition = crate::session::ProviderCallDisposition::ToolRequested;
    record.requested_tools = 1;
    let mut intent = fixture
        .events
        .iter()
        .rev()
        .find_map(|event| match event {
            Event::ToolFinished { observation, .. }
                if observation.intent.agent_run_id == fixture.primary =>
            {
                Some(observation.intent.clone())
            }
            _ => None,
        })
        .unwrap();
    intent.id = uuid::Uuid::now_v7();
    intent.call = crate::tools::ToolCall::Write {
        path: "src/a".into(),
        expected_digest: Some(crate::validate::hex_digest(b"new")),
        content: "changed".into(),
    };
    fixture.events.push(if started {
        Event::ToolStarted {
            run_id: fixture.run,
            agent_run_id: fixture.primary,
            intent: intent.into(),
            active_ceiling_ms: 0,
            verdict: None,
        }
    } else {
        Event::ToolProposed {
            run_id: fixture.run,
            agent_run_id: fixture.primary,
            intent: intent.into(),
            active_ceiling_ms: 0,
        }
    });
    (fixture.session, fixture.events)
}

#[test]
fn rewind_after_a_proposal_only_publication_survives_cold_reopen() {
    for started in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let (session, events) = pending_publication_history(started);
        {
            let mut connection = open_connection(&root, false).unwrap();
            let mut hot = None;
            for event in events {
                append(&mut connection, &mut hot, session, event).unwrap();
            }
        }
        let mut connection = open_connection(&root, false).unwrap();
        let (_, view) = resolve_view(&connection, session, 0, &mut 0).unwrap();
        let view = view.unwrap();
        let target = view.rewinds.checkpoints[0].clone();
        let plan = crate::session::rewind::plan(&view, &target).unwrap();
        assert_eq!(!plan.blockers.is_empty(), started);
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].path, "src/a");
        assert_eq!(plan.files[0].restore_to, None);
        assert_eq!(
            plan.files[0].expected_current.as_ref().unwrap().sha256,
            crate::validate::hex_digest(b"new")
        );
        let begin = crate::session::rewind_started_fixture(&view, target);
        assert_eq!(
            append(&mut connection, &mut None, session, begin).is_ok(),
            !started
        );
    }
}

fn forked_rewind_fixture(
    root: &StateRoot,
) -> (Connection, Option<HotSession>, SessionId, SessionView) {
    let f = crate::session::worker_protocol_fixture();
    let mut connection = open_connection(root, false).unwrap();
    let mut hot = None;
    for event in &f.events {
        append(&mut connection, &mut hot, f.session, event.clone()).unwrap();
    }
    let child = fork_session(&mut connection, f.session, "Fork".into()).unwrap();
    let (_, view) = resolve_view(&connection, child, 0, &mut 0).unwrap();
    (connection, hot, child, view.unwrap())
}

#[test]
fn fork_inspection_preserves_sqlite_failures() {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&temp.path().join("state")).unwrap();
    let (connection, _, child, _) = forked_rewind_fixture(&root);
    let inspection = super::super::replay::inspect_session(&connection, child).unwrap();
    assert!(inspection.failure.is_none());
    assert!(inspection.prefix_view.is_some());
    connection
        .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 1)
        .unwrap();
    assert!(matches!(
        super::super::replay::inspect_session(&connection, child),
        Err(StoreError::Sqlite(_))
    ));
}

#[test]
fn rewind_right_after_a_fork_survives_cold_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&temp.path().join("state")).expect("private rewind State root");
    let (mut connection, mut hot, child, view) = forked_rewind_fixture(&root);
    let event = crate::session::rewind_started_fixture(&view, view.rewinds.checkpoints[0].clone());
    append(&mut connection, &mut hot, child, event).unwrap();
    let (_, view) = resolve_view(&connection, child, 0, &mut 0).unwrap();
    let event = crate::session::rewind_finished_fixture(
        &view.unwrap(),
        crate::session::rewind::RewindOutcome::Restored,
    );
    append(&mut connection, &mut hot, child, event).unwrap();
    drop(hot);
    let cold = open_connection(&root, true).unwrap();
    let (_, view) = resolve_view(&cold, child, 0, &mut 0).unwrap();
    assert_eq!(
        view.unwrap().rewinds.operations[0].outcome(),
        crate::session::rewind::RewindOutcome::Restored
    );
}

#[test]
fn forged_rewind_workspace_identity_is_rejected_on_append_and_replay() {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&temp.path().join("state")).expect("private rewind State root");
    let (mut connection, mut hot, child, view) = forked_rewind_fixture(&root);
    let event = crate::session::rewind_started_fixture(&view, view.rewinds.checkpoints[0].clone());
    let Event::WorkspaceRewindStarted { record } = &event else {
        unreachable!()
    };
    let mut forged = record.clone();
    forged.workspace_inode += 1;
    assert!(
        append(
            &mut connection,
            &mut hot,
            child,
            Event::WorkspaceRewindStarted { record: forged }
        )
        .is_err()
    );
    append(&mut connection, &mut hot, child, event).unwrap();
    drop(hot);
    let payload: String = connection
        .query_row(
            "SELECT payload FROM events WHERE kind='WorkspaceRewindStarted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let inode = view.workspace_identity.unwrap().inode;
    let tampered = payload.replace(
        &format!("\"workspace_inode\":{inode}"),
        &format!("\"workspace_inode\":{}", inode + 1),
    );
    assert_ne!(tampered, payload);
    connection
        .execute(
            "UPDATE events SET payload=?1 WHERE kind='WorkspaceRewindStarted'",
            [tampered],
        )
        .unwrap();
    let cold = open_connection(&root, true).unwrap();
    assert!(resolve_view(&cold, child, 0, &mut 0).is_err());
}

fn seed_session(
    connection: &mut Connection,
    _hot_session: &mut Option<HotSession>,
    identity: WorkspaceIdentity,
    renames: usize,
) -> SessionId {
    let session_id = SessionId::new();
    let start = Event::SessionStarted {
        title: "Session".into(),
        workspace_identity: identity,
    };
    let start_payload = start.payload().expect("bounded Event payload");
    connection
        .execute(
            "INSERT INTO events (sequence, session_id, kind, payload, created_at_ms)
             VALUES ((SELECT COALESCE(MAX(sequence), 0) + 1 FROM events), ?1, ?2, ?3, 0)",
            params![session_id.to_string(), start.kind(), start_payload],
        )
        .expect("first Event");
    let rename = Event::SessionRenamed { title: "x".into() };
    let payload = rename.payload().expect("bounded Event payload");
    let transaction = connection.transaction().expect("seed transaction");
    for _ in 0..renames {
        transaction
            .execute(
                "INSERT INTO events (sequence, session_id, kind, payload, created_at_ms)
                 VALUES ((SELECT MAX(sequence) + 1 FROM events), ?1, ?2, ?3, 0)",
                params![session_id.to_string(), rename.kind(), payload],
            )
            .expect("valid Event row");
    }
    transaction.commit().expect("seeded history");
    session_id
}

#[test]
fn listing_is_filtered_by_workspace_before_decoding_history() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let mut hot_session = None;
    let other = WorkspaceIdentity {
        device: 1,
        inode: 1,
    };
    let empty = WorkspaceIdentity {
        device: 1,
        inode: 2,
    };
    for _ in 0..20 {
        seed_session(&mut connection, &mut hot_session, other, 500);
    }
    let (page, _) =
        list_session_page(&connection, empty, None, 50, 10, None).expect("empty Workspace");
    assert!(page.items.is_empty() && !page.more);
}

#[test]
fn budget_limited_listing_pages_pin_watermark_without_skips_or_duplicates() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let mut hot_session = None;
    let mine = WorkspaceIdentity {
        device: 1,
        inode: 1,
    };
    let other = WorkspaceIdentity {
        device: 1,
        inode: 2,
    };
    let mut expected = Vec::new();
    for round in 0..7 {
        expected.push(seed_session(&mut connection, &mut hot_session, mine, 30));
        seed_session(&mut connection, &mut hot_session, other, round);
    }
    expected.reverse();
    let mut listed = Vec::new();
    let mut after = None;
    let reader = open_connection(&root, true).expect("read-only store");
    let mut pages = 0;
    let mut watermark = None;
    loop {
        let (page, sequence) =
            list_session_page(&reader, mine, after, 100, 40, watermark).expect("bounded page");
        assert!(watermark.is_none_or(|pinned| pinned == sequence));
        watermark = Some(sequence);
        assert!(page.items.iter().all(|item| item.title == "x"));
        assert!(
            !page.items.is_empty() && page.items.len() < 7,
            "budget bounds each page"
        );
        after = page.items.last().map(|item| (item.id, item.last_sequence));
        listed.extend(page.items.iter().map(|item| item.id));
        pages += 1;
        if pages == 1 {
            let anchor = after.expect("first page anchor").0;
            for id in [anchor, *expected.last().expect("unlisted Session")] {
                append(
                    &mut connection,
                    &mut hot_session,
                    id,
                    Event::SessionRenamed {
                        title: "later".into(),
                    },
                )
                .expect("writer advances anchor and unlisted Session between pages");
            }
            seed_session(&mut connection, &mut hot_session, mine, 0);
            assert!(matches!(
                list_session_page(&reader, mine, after, 100, 40, None),
                Err(StoreError::StaleCursor)
            ));
        }
        if !page.more {
            break;
        }
    }
    assert!(pages > 1);
    assert_eq!(listed, expected);
    assert!(matches!(
        list_session_page(&connection, mine, Some((expected[0], 1)), 100, 40, None),
        Err(StoreError::StaleCursor)
    ));
    let (first, _) =
        list_session_page(&connection, mine, None, 3, usize::MAX, None).expect("limit page");
    assert_eq!(first.items.len(), 3);
    assert!(first.more);
}

#[test]
fn listing_page_orders_and_reads_from_one_snapshot() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let mut hot_session = None;
    let mine = WorkspaceIdentity {
        device: 1,
        inode: 1,
    };
    let older = seed_session(&mut connection, &mut hot_session, mine, 3);
    let newer = seed_session(&mut connection, &mut hot_session, mine, 3);
    let old_tail: i64 = connection
        .query_row(
            "SELECT MAX(sequence) FROM events WHERE session_id = ?1",
            [older.to_string()],
            |row| row.get(0),
        )
        .expect("tail");
    let reader = open_connection(&root, true).expect("read-only store");
    let mut writer_state = None;
    let (page, _) = list_page_in_snapshot(&reader, mine, None, 10, usize::MAX, None, || {
        let mut writer = open_connection(&root, false).expect("concurrent writer");
        append(
            &mut writer,
            &mut writer_state,
            older,
            Event::SessionRenamed {
                title: "later".into(),
            },
        )
        .expect("listing never blocks the writer");
    })
    .expect("bounded page");
    assert_eq!(
        page.items.iter().map(|item| item.id).collect::<Vec<_>>(),
        [newer, older]
    );
    assert_eq!(page.items[1].last_sequence, old_tail as u64);
    let (relisted, _) =
        list_session_page(&reader, mine, None, 10, usize::MAX, None).expect("fresh page");
    assert_eq!(relisted.items[0].id, older);
}

#[test]
fn listing_waits_past_the_old_bound_for_a_committing_writer_then_reports_busy() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let mut hot_session = None;
    let mine = WorkspaceIdentity {
        device: 1,
        inode: 1,
    };
    seed_session(&mut connection, &mut hot_session, mine, 3);
    let reader = open_connection(&root, true).expect("read-only store");
    connection
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("writer commit lock");
    let (started, ready) = std::sync::mpsc::sync_channel(0);
    let listing = std::thread::spawn(move || {
        started.send(()).expect("listing started");
        let page = list_session_page(&reader, mine, None, 10, usize::MAX, None);
        (page, reader)
    });
    ready.recv().expect("listing started before commit");
    std::thread::sleep(std::time::Duration::from_millis(600));
    connection.execute_batch("COMMIT").expect("commit");
    let (page, reader) = listing.join().expect("listing thread");
    let (page, _) = page.expect("a committing writer is waited for beyond 250 ms");
    assert_eq!(page.items.len(), 1);
    reader
        .busy_timeout(std::time::Duration::from_millis(20))
        .expect("short bound");
    connection
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("writer commit lock");
    let refused = list_session_page(&reader, mine, None, 10, usize::MAX, None);
    connection.execute_batch("COMMIT").expect("commit");
    assert!(matches!(refused, Err(StoreError::ListingBusy)));
}

#[test]
fn long_listings_never_block_concurrent_appends() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let mut hot_session = None;
    let mine = WorkspaceIdentity {
        device: 1,
        inode: 1,
    };
    for _ in 0..40 {
        seed_session(&mut connection, &mut hot_session, mine, 1_000);
    }
    let target = seed_session(&mut connection, &mut hot_session, mine, 0);
    let reader = open_connection(&root, true).expect("read-only store");
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let (started, ready) = std::sync::mpsc::sync_channel(0);
    let listing = std::thread::spawn(move || {
        let mut pages = 0;
        while pages == 0 || !stopping.load(Ordering::SeqCst) {
            let (page, _) =
                list_page_in_snapshot(&reader, mine, None, 100, usize::MAX, None, || {
                    if pages == 0 {
                        started.send(()).expect("listing started");
                    }
                })
                .expect("page");
            assert_eq!(page.items.len(), 41);
            pages += 1;
        }
        pages
    });
    ready.recv().expect("listing started before appends");
    let appended = (0..60).try_for_each(|index| {
        append(
            &mut connection,
            &mut hot_session,
            target,
            Event::SessionRenamed {
                title: format!("t{index}"),
            },
        )
        .map(|_| ())
    });
    stop.store(true, Ordering::SeqCst);
    assert!(listing.join().expect("listing thread") >= 1);
    appended.expect("append commits durably without SQLITE_BUSY while listing runs");
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1",
            [target.to_string()],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(count, 61);
}

#[test]
fn listing_retries_busy_reads_at_the_same_watermark_and_stops_at_the_bound() {
    use std::cell::{Cell, RefCell};

    thread_local! {
        static WRITER: RefCell<Option<(Connection, SessionId)>> = const { RefCell::new(None) };
        static RELEASE_AT: Cell<usize> = const { Cell::new(0) };
        static BUSY: Cell<usize> = const { Cell::new(0) };
    }

    let temp = tempfile::tempdir().expect("journal fixture");
    let path = temp.path().join(DATABASE_FILE);
    Connection::open(&path).expect("create journal");
    let mut writer = open_database(&path, false).expect("writer");
    let mine = WorkspaceIdentity {
        device: 1,
        inode: 1,
    };
    let target = seed_session(&mut writer, &mut None, mine, 0);
    writer
        .busy_timeout(std::time::Duration::ZERO)
        .expect("nonblocking writer");
    let reader = open_database(&path, true).expect("reader");
    reader
        .busy_handler(Some(|_| {
            BUSY.set(BUSY.get() + 1);
            if BUSY.get() == RELEASE_AT.get() {
                WRITER.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    let (writer, target) = slot.as_mut().expect("writer at commit gate");
                    writer.execute_batch("COMMIT").expect("release commit lock");
                    append(
                        writer,
                        &mut None,
                        *target,
                        Event::SessionRenamed {
                            title: "committed during contention".into(),
                        },
                    )
                    .expect("reader holds no lock while retrying");
                });
            }
            false
        }))
        .expect("deterministic busy boundary");
    WRITER.with(|slot| *slot.borrow_mut() = Some((writer, target)));

    for iteration in 0..30 {
        let watermark: i64 = reader
            .query_row(WATERMARK_SQL, [], |row| row.get(0))
            .expect("watermark");
        let watermark = u64::try_from(watermark).expect("nonnegative sequence");
        BUSY.set(0);
        let busy_reads = iteration % 3 + 1;
        RELEASE_AT.set(busy_reads);
        let result = list_page_in_snapshot(&reader, mine, None, 10, usize::MAX, None, || {
            WRITER.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .expect("writer")
                    .0
                    .execute_batch("BEGIN EXCLUSIVE")
                    .expect("hold commit lock")
            });
        });
        let (page, sequence) = result.expect("page survives a completed busy wait");
        assert_eq!(BUSY.get(), busy_reads);
        assert_eq!(sequence, watermark);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].last_sequence, watermark);
    }

    BUSY.set(0);
    RELEASE_AT.set(0);
    WRITER.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("writer")
            .0
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("hold commit lock")
    });
    let refused = list_all_sessions(&reader, mine);
    WRITER.with(|slot| {
        slot.borrow_mut()
            .take()
            .expect("writer")
            .0
            .execute_batch("COMMIT")
            .expect("release commit lock")
    });
    assert!(matches!(refused, Err(StoreError::ListingBusy)));
    assert_eq!(BUSY.get(), 4, "one attempt and three retries");
}

#[test]
fn listing_holds_no_reader_lock_while_decoding() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let mut hot_session = None;
    let mine = WorkspaceIdentity {
        device: 1,
        inode: 1,
    };
    let other = WorkspaceIdentity {
        device: 1,
        inode: 2,
    };
    for _ in 0..3 {
        seed_session(&mut connection, &mut hot_session, mine, 600);
    }
    let bystander = seed_session(&mut connection, &mut hot_session, other, 0);
    let reader = open_connection(&root, true).expect("read-only store");
    let writer_root = root.try_clone().expect("state clone");
    let appended = Arc::new(AtomicUsize::new(0));
    let counter = appended.clone();
    BEFORE_DECODE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let mut writer = open_connection(&writer_root, false).expect("writer");
            writer
                .busy_timeout(std::time::Duration::ZERO)
                .expect("no wait");
            append(
                &mut writer,
                &mut None,
                bystander,
                Event::SessionRenamed { title: "y".into() },
            )
            .expect("append succeeds immediately while the listing decodes");
            counter.fetch_add(1, Ordering::SeqCst);
        }));
    });
    let (page, _) = list_session_page(&reader, mine, None, 100, usize::MAX, None).expect("page");
    BEFORE_DECODE.with(|hook| *hook.borrow_mut() = None);
    assert_eq!(page.items.len(), 3);
    assert!(appended.load(Ordering::SeqCst) > 3);
}

#[test]
fn fork_resolution_ignores_events_committed_after_the_watermark() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let mut hot_session = None;
    let fixture = crate::session::worker_protocol_fixture();
    let source = fixture.session;
    let mut mine = None;
    for event in &fixture.events {
        if let Event::SessionStarted {
            workspace_identity, ..
        } = event
        {
            mine = Some(*workspace_identity);
        }
        append(&mut connection, &mut hot_session, source, event.clone()).expect("fixture Event");
    }
    let mine = mine.expect("fixture identity");
    let tail: i64 = connection
        .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
        .expect("tail");
    let reader = open_connection(&root, true).expect("read-only store");
    let writer_root = root.try_clone().expect("state clone");
    let (page, _) = list_page_in_snapshot(&reader, mine, None, 10, usize::MAX, None, || {
        let mut writer = open_connection(&writer_root, false).expect("writer");
        append(
            &mut writer,
            &mut None,
            source,
            Event::SessionRenamed {
                title: "newer".into(),
            },
        )
        .expect("source advances");
        fork_session(&mut writer, source, "Child".into()).expect("child forked later");
    })
    .expect("page at the captured watermark");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, source);
    assert_eq!(page.items[0].last_sequence, tail as u64);
    let (later, _) =
        list_session_page(&reader, mine, None, 10, usize::MAX, None).expect("later page");
    assert_eq!(later.items.len(), 2);
}

#[test]
fn every_listing_statement_is_an_index_seek_without_scan_or_temp_sort() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let connection = open_connection(&root, false).expect("file-backed store");
    for (sql, arguments) in [
        (WATERMARK_SQL, 0),
        (NEXT_HEAD_SQL, 1),
        (HEAD_TAIL_SQL, 2),
        (HEAD_FIRST_SQL, 2),
        (LINEAGE_COUNT_SQL, 2),
        (LANE_SQL, 3),
    ] {
        let values = vec![rusqlite::types::Value::Integer(1); arguments];
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("plan");
        let plan: Vec<String> = statement
            .query_map(rusqlite::params_from_iter(values), |row| row.get(3))
            .expect("plan rows")
            .collect::<Result<_, _>>()
            .expect("plan detail");
        assert!(!plan.is_empty());
        for detail in plan {
            assert!(
                detail.starts_with("SEARCH") && !detail.contains("TEMP B-TREE"),
                "{sql}: {detail}"
            );
        }
    }
}

#[test]
fn full_listing_of_a_large_store_never_fails_a_concurrent_append() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let mut hot_session = None;
    let mine = WorkspaceIdentity {
        device: 1,
        inode: 1,
    };
    let mut last = None;
    for _ in 0..500 {
        last = Some(seed_session(&mut connection, &mut hot_session, mine, 99));
    }
    let target = last.expect("seeded");
    let reader = open_connection(&root, true).expect("read-only store");
    let listing = std::thread::spawn(move || {
        let mut after = None;
        let mut seen = std::collections::HashSet::new();
        let mut watermark = None;
        loop {
            let (page, sequence) =
                list_session_page(&reader, mine, after, 100, usize::MAX, watermark).expect("page");
            watermark = Some(sequence);
            for item in &page.items {
                assert!(seen.insert(item.id), "duplicate Session across pages");
            }
            after = page.items.last().map(|item| (item.id, item.last_sequence));
            if !page.more {
                break;
            }
        }
        assert_eq!(seen.len(), 500);

        let decodes = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = decodes.clone();
        BEFORE_DECODE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || counter.set(counter.get() + 1)));
        });
        let items = list_all_sessions(&reader, mine).expect("full listing");
        BEFORE_DECODE.with(|hook| *hook.borrow_mut() = None);
        assert_eq!(
            decodes.get(),
            501,
            "one head-discovery batch and one replay chunk per Session across five pages"
        );
        let mut full = std::collections::HashSet::new();
        for item in items {
            assert!(full.insert(item.id), "duplicate Session in full listing");
        }
        assert_eq!(full, seen);
        full.len()
    });
    for index in 0..40 {
        append(
            &mut connection,
            &mut hot_session,
            target,
            Event::SessionRenamed {
                title: format!("t{index}"),
            },
        )
        .expect("append at the normal busy timeout never fails during a listing");
    }
    assert_eq!(listing.join().expect("listing thread"), 500);
}

#[test]
fn fork_write_queries_search_indexes() {
    let temp = tempfile::tempdir().unwrap();
    let mut connection = Connection::open(temp.path().join(DATABASE_FILE)).unwrap();
    connection.execute_batch(SCHEMA_SQL).unwrap();
    let transaction = begin_write(&mut connection, None).unwrap();
    for (sql, index) in [
        (super::super::replay::RUN_OWNER_SQL, Some("events_by_run")),
        (
            super::super::replay::SESSION_EVENTS_SQL,
            Some("events_by_session"),
        ),
        (NEXT_SEQUENCE_SQL, None),
    ] {
        let mut statement = transaction
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let parameters = vec!["0"; statement.parameter_count()];
        let plan = statement
            .query_map(rusqlite::params_from_iter(parameters), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            plan.iter().any(|step| {
                step.starts_with("SEARCH events") && index.is_none_or(|index| step.contains(index))
            }),
            "indexed search for {sql}: {plan:?}"
        );
        assert!(
            plan.iter().all(|step| !step.starts_with("SCAN events")),
            "no whole-table scan for {sql}: {plan:?}"
        );
    }
    assert_eq!(
        super::super::replay::session_for_run(&transaction, crate::session::RunId::new()).unwrap(),
        None
    );
    transaction.commit().unwrap();
}

#[test]
fn fork_rejects_ambiguous_run_owners() {
    let temp = tempfile::tempdir().unwrap();
    let mut connection = Connection::open(temp.path().join(DATABASE_FILE)).unwrap();
    connection.execute_batch(SCHEMA_SQL).unwrap();
    let f = crate::session::worker_protocol_fixture();
    let mut hot = None;
    for event in &f.events {
        append(&mut connection, &mut hot, f.session, event.clone()).unwrap();
    }
    let (_, view) = resolve_view(&connection, f.session, 0, &mut 0).unwrap();
    let run = view.unwrap().runs.last().unwrap().clone();
    let sequence = run.finished_sequence.unwrap();
    assert_eq!(
        prepare_fork(&mut connection, hot.as_ref(), f.session, Some(sequence))
            .unwrap()
            .source_session_id,
        f.session
    );
    connection
        .execute(
            "INSERT INTO events
         SELECT (SELECT MAX(sequence) + 1 FROM events), ?1, run_id, agent_run_id, kind,
                payload, created_at_ms
         FROM events WHERE run_id=?2 ORDER BY sequence LIMIT 1",
            params![SessionId::new().to_string(), run.id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        prepare_fork(&mut connection, hot.as_ref(), f.session, Some(sequence)),
        Err(StoreError::InvalidHistory)
    ));
}

#[test]
fn earlier_fork_revalidates_prepared_prefix_and_event_capacity_before_writing() {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&temp.path().join("state")).unwrap();
    let mut connection = open_connection(&root, false).unwrap();
    let f = crate::session::worker_protocol_fixture();
    let mut hot = None;
    for event in &f.events {
        append(&mut connection, &mut hot, f.session, event.clone()).unwrap();
    }
    let (events, view) = resolve_view(&connection, f.session, 0, &mut 0).unwrap();
    let sequence = view
        .unwrap()
        .runs
        .last()
        .unwrap()
        .finished_sequence
        .unwrap();
    let boundary = prepare_fork(&mut connection, hot.as_ref(), f.session, Some(sequence)).unwrap();
    let count = || {
        connection
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    let before = count();
    connection
        .execute(
            "UPDATE events SET payload=json_set(payload, '$.title', 'Changed') WHERE sequence=?1",
            [events[0].sequence as i64],
        )
        .unwrap();
    assert!(matches!(
        fork_at(&mut connection, hot.as_ref(), boundary, "Refused".into()),
        Err(StoreError::InvalidHistory)
    ));
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        before
    );
    connection
        .execute(
            "UPDATE events SET payload=?1 WHERE sequence=?2",
            params![
                events[0].event.payload().unwrap(),
                events[0].sequence as i64
            ],
        )
        .unwrap();
    let rename = Event::SessionRenamed {
        title: "Capacity".into(),
    };
    let transaction = connection.transaction().unwrap();
    for next in events.len() + 1..MAX_SESSION_EVENTS {
        transaction.execute("INSERT INTO events (sequence, session_id, kind, payload, created_at_ms) VALUES (?1, ?2, ?3, ?4, 0)",
            params![next as i64, f.session.to_string(), rename.kind(), rename.payload().unwrap()]).unwrap();
    }
    transaction.commit().unwrap();
    assert_eq!(
        prepare_fork(&mut connection, hot.as_ref(), f.session, Some(sequence)).unwrap(),
        boundary,
        "exactly one fork Event fits"
    );
    append(&mut connection, &mut hot, f.session, rename).unwrap();
    assert!(matches!(
        prepare_fork(&mut connection, hot.as_ref(), f.session, Some(sequence)),
        Err(StoreError::ReplayLimit)
    ));
    assert!(matches!(
        fork_at(&mut connection, hot.as_ref(), boundary, "Refused".into()),
        Err(StoreError::ReplayLimit)
    ));
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        MAX_SESSION_EVENTS as i64
    );
}

#[test]
fn listing_reads_earlier_boundary_forks_at_the_watermark() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let mut hot = None;
    let fixture = crate::session::worker_protocol_fixture();
    let mut mine = None;
    for event in &fixture.events {
        if let Event::SessionStarted {
            workspace_identity, ..
        } = event
        {
            mine = Some(*workspace_identity);
        }
        append(&mut connection, &mut hot, fixture.session, event.clone()).expect("fixture Event");
    }
    let mine = mine.expect("fixture identity");
    let (_, view) = resolve_view(&connection, fixture.session, 0, &mut 0).expect("source view");
    let boundary_sequence = view
        .expect("source view")
        .runs
        .first()
        .and_then(|run| run.finished_sequence)
        .expect("finished Run");
    let boundary = prepare_fork(
        &mut connection,
        hot.as_ref(),
        fixture.session,
        Some(boundary_sequence),
    )
    .expect("boundary");
    let child =
        fork_at(&mut connection, hot.as_ref(), boundary, "Earlier".into()).expect("earlier fork");
    let reader = open_connection(&root, true).expect("read-only store");
    let writer_root = root.try_clone().expect("state clone");
    let source = fixture.session;
    let (page, _) = list_page_in_snapshot(&reader, mine, None, 10, usize::MAX, None, || {
        let mut writer = open_connection(&writer_root, false).expect("writer");
        append(
            &mut writer,
            &mut None,
            source,
            Event::SessionRenamed {
                title: "later".into(),
            },
        )
        .expect("source advances after the watermark");
    })
    .expect("page");
    let ids: Vec<_> = page.items.iter().map(|item| item.id).collect();
    assert!(ids.contains(&child) && ids.contains(&source));
    let listed = page.items.iter().find(|item| item.id == child).unwrap();
    assert_eq!(listed.title, "Earlier");
}
