use super::super::replay::{
    BEFORE_DECODE, HEAD_FIRST_SQL, HEAD_TAIL_SQL, LANE_SQL, LINEAGE_COUNT_SQL, NEXT_HEAD_SQL,
    WATERMARK_SQL, list_all_sessions, list_page_in_snapshot, list_session_page,
};
use super::*;
use crate::session::WorkspaceIdentity;
use crate::session::payload_version;

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
                "INSERT INTO events (sequence, session_id, kind, event_version, payload, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            )
            .expect("seed statement");
        for sequence in 2..=MAX_SESSION_EVENTS {
            insert
                .execute(params![
                    sequence as i64,
                    session_id.to_string(),
                    rename.kind(),
                    rename.version(),
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

#[test]
fn other_connection_changes_invalidate_hot_history() {
    let temp = tempfile::tempdir().expect("private test root");
    let path = temp.path().join(DATABASE_FILE);
    let mut connection = Connection::open(&path).expect("file-backed store");
    connection
        .execute_batch(SCHEMA_SQL)
        .expect("journal schema");
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

    let second = Connection::open(&path).expect("second connection");
    let other_id = SessionId::new();
    let other_start = Event::SessionStarted {
        title: "Other".into(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 1,
        },
    };
    second
        .execute(
            "INSERT INTO events (sequence, session_id, kind, event_version, payload, created_at_ms)
             VALUES (2, ?1, ?2, ?3, ?4, 0)",
            params![
                other_id.to_string(),
                other_start.kind(),
                other_start.version(),
                other_start.payload().expect("other start payload"),
            ],
        )
        .expect("other committed Session");
    thread_local! {
        static WRITER: std::cell::RefCell<Option<Connection>> = const { std::cell::RefCell::new(None) };
    }
    second
        .execute_batch("BEGIN IMMEDIATE; UPDATE events SET created_at_ms=1 WHERE sequence=2;")
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
    let refused = append(
        &mut connection,
        &mut hot_session,
        session_id,
        Event::SessionRenamed {
            title: "Racing".into(),
        },
    );
    connection.busy_handler(None).expect("remove test handler");
    let second = WRITER.with(|writer| writer.borrow_mut().take()).unwrap();
    assert!(
        matches!(refused, Err(StoreError::HistoryChanged)),
        "expected HistoryChanged, got {refused:?}"
    );
    assert!(hot_session.is_none());
    assert!(connection.is_autocommit());
    let committed = append(
        &mut connection,
        &mut hot_session,
        session_id,
        Event::SessionRenamed {
            title: "Updated".into(),
        },
    )
    .expect("append after another writer");
    assert_eq!(committed.sequence, 3);
    assert!(hot_session.is_some());

    second
        .execute("UPDATE events SET kind='UnknownKind' WHERE sequence=2", [])
        .expect("corrupt unrelated committed Event");
    assert!(matches!(
        append(
            &mut connection,
            &mut hot_session,
            session_id,
            Event::SessionRenamed {
                title: "Rejected".into(),
            },
        ),
        Err(StoreError::InvalidHistory)
    ));
    assert!(hot_session.is_none());
    second
        .execute(
            "UPDATE events SET kind=?1 WHERE sequence=2",
            [other_start.kind()],
        )
        .expect("restore test-owned Event");
    let recovered = append(
        &mut connection,
        &mut hot_session,
        session_id,
        Event::SessionRenamed {
            title: "Recovered".into(),
        },
    )
    .expect("full replay after invalidation");
    assert_eq!(recovered.sequence, 4);
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
fn run_start_admission_reserves_the_current_version_payload_exceptions() {
    let message = payload_version("MessageAccepted").expect("payload version");
    let goal = payload_version("GoalStarted").expect("payload version");
    assert_eq!(
        crate::store::RUN_START_EXTRA_BYTES,
        event_payload_limit("MessageAccepted", message) + event_payload_limit("GoalStarted", goal)
            - 2 * crate::store::MAX_EVENT_BYTES
    );
    assert_eq!(
        event_payload_limit("MessageAccepted", message - 1),
        crate::store::MAX_EVENT_BYTES
    );
}

// The largest typed Write: a six-byte escape per content byte and a long escaped path.
fn largest_tool_started_payload() -> String {
    let intent = crate::tools::EffectIntent {
        host: None,
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
        expires_at_ms: u64::MAX,
        use_count: 1,
    };
    assert!(intent.valid());
    Event::ToolStarted {
        run_id: intent.run_id,
        agent_run_id: intent.agent_run_id,
        intent,
        active_ceiling_ms: u64::MAX,
    }
    .payload()
    .unwrap()
}

// Near the floor, one admitted intent (its ToolStarted and ToolFinished slots) must leave room to
// commit the largest ToolStarted and a full ToolFinished; reserving the completion alone does not.
#[test]
fn tool_admission_near_capacity_reserves_the_largest_intent_and_its_completion() {
    let temp = tempfile::tempdir().expect("private test root");
    let root = StateRoot::admit(&temp.path().join("state")).expect("admitted state");
    let mut connection = open_connection(&root, false).expect("file-backed store");
    let session_id = SessionId::new();
    let started = largest_tool_started_payload();
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
        let results = [false, true].map(|sealed| {
            admit_in_transaction(t, session_id, 2, observations, false, sealed).is_ok()
        });
        assert_eq!(
            results[0], results[1],
            "worker sealed-data admission reserves the same bytes"
        );
        results[0]
    };
    let pair_commits = |t: &Transaction<'_>| {
        t.execute_batch("SAVEPOINT pair").unwrap();
        for (kind, payload) in [("ToolStarted", &started), ("ToolFinished", &finished)] {
            t.execute(
                "INSERT INTO events (session_id, run_id, agent_run_id, kind, event_version, payload, created_at_ms) VALUES (?1, 'run', 'agent', ?2, 1, ?3, 0)",
                params![session_id.to_string(), kind, payload],
            )
            .unwrap();
        }
        let room = MAX_DATABASE_BYTES - pages(t) * page_size;
        t.execute_batch("ROLLBACK TO pair; RELEASE pair").unwrap();
        room >= ADMISSION_HEADROOM_BYTES
    };
    // Pad, then step one page at a time to the last page count each reservation admits.
    let pad = |t: &Transaction<'_>, rows: i64| {
        t.execute(
            "WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n < ?1) INSERT INTO capacity_padding SELECT zeroblob(3000) FROM rows",
            [rows],
        )
        .unwrap();
    };
    let last_admitted = |t: &Transaction<'_>, observations: usize| {
        assert!(admits(t, observations));
        loop {
            t.execute_batch("SAVEPOINT step").unwrap();
            pad(t, 1);
            if !admits(t, observations) {
                t.execute_batch("ROLLBACK TO step; RELEASE step").unwrap();
                return;
            }
            t.execute_batch("RELEASE step").unwrap();
        }
    };
    transaction
        .execute_batch("CREATE TABLE capacity_padding(value BLOB NOT NULL) STRICT;")
        .unwrap();
    let floor = (MAX_DATABASE_BYTES - ADMISSION_HEADROOM_BYTES) / page_size;
    pad(
        &transaction,
        floor - pages(&transaction) - 2 * 1024 * 1024 / page_size,
    );
    last_admitted(&transaction, 2);
    assert!(pair_commits(&transaction));
    pad(&transaction, 1);
    assert!(
        !admits(&transaction, 2),
        "the intent is refused before dispatch"
    );
    last_admitted(&transaction, 1);
    assert!(
        !pair_commits(&transaction),
        "a completion-only reservation would strand the intent"
    );
}

#[test]
fn every_event_slot_admits_a_current_tool_observation() {
    let supervisor = payload_version("SupervisorVerdictRecorded").unwrap();
    assert_eq!(
        event_payload_limit("SupervisorVerdictRecorded", supervisor),
        2048
    );
    assert_eq!(
        event_payload_limit("SupervisorVerdictRecorded", supervisor + 1),
        crate::store::MAX_EVENT_BYTES
    );
    let finished = payload_version("ToolFinished").expect("payload version");
    assert_eq!(
        event_payload_limit("ToolFinished", finished),
        crate::tools::types::MAX_TOOL_OBSERVATION_BYTES + 512
    );
    assert_eq!(
        event_payload_limit("ToolFinished", finished - 1),
        crate::store::MAX_EVENT_BYTES
    );
    let started = payload_version("ToolStarted").expect("payload version");
    assert_eq!(
        event_payload_limit("ToolStarted", started - 1),
        crate::store::MAX_EVENT_BYTES
    );
    let payload = largest_tool_started_payload();
    assert!(payload.len() > crate::store::MAX_EVENT_BYTES);
    assert!(payload.len() <= event_payload_limit("ToolStarted", started));
    let verdict = payload_version("ToolPermissionDecided").expect("payload version");
    assert_eq!(
        event_payload_limit("ToolPermissionDecided", verdict),
        crate::store::MAX_EVENT_BYTES
    );
}

#[test]
fn worker_protocol_store_cold_prefixes_and_old_versions_preserve_data() {
    for f in [
        crate::session::worker_protocol_fixture(),
        crate::session::worker_command_protocol_fixture(),
    ] {
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
        drop(hot);
        for kind in [
            "RunStarted",
            "AgentSpawned",
            "ProviderCallRecorded",
            "AgentFinished",
            "ToolStarted",
            "ToolFinished",
            "WorkerDataChunk",
            "WorkerDataSealed",
            "WorkerHandoverRecorded",
            "WorkerIntegrationRecorded",
            "WorkerLeaseReleased",
            "WorkerVerificationRecorded",
        ] {
            let current = payload_version(kind).unwrap();
            // A version-1 kind has no previous version; a foreign version must reject just the same.
            let stale = if current > 1 {
                current - 1
            } else {
                current + 1
            };
            connection
                .execute(
                    "UPDATE events SET event_version=?1 WHERE kind=?2",
                    params![stale, kind],
                )
                .unwrap();
            let before = std::fs::read(root.path().join(DATABASE_FILE)).unwrap();
            let cold = open_connection(&root, true).unwrap();
            assert!(
                resolve_view(&cold, f.session, 0, &mut 0).is_err(),
                "old {kind}"
            );
            drop(cold);
            assert_eq!(
                std::fs::read(root.path().join(DATABASE_FILE)).unwrap(),
                before,
                "unchanged {kind}"
            );
            connection
                .execute(
                    "UPDATE events SET event_version=?1 WHERE kind=?2",
                    params![current, kind],
                )
                .unwrap();
        }
    }
}

#[test]
fn rewind_store_cold_prefixes_and_unsupported_versions_preserve_data() {
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
    for kind in ["WorkspaceRewindStarted", "WorkspaceRewindFinished"] {
        {
            connection
                .execute("UPDATE events SET event_version=2 WHERE kind=?1", [kind])
                .unwrap();
            let before = std::fs::read(root.path().join(DATABASE_FILE)).unwrap();
            let cold = open_connection(&root, true).unwrap();
            assert!(resolve_view(&cold, f.session, 0, &mut 0).is_err());
            drop(cold);
            assert_eq!(
                std::fs::read(root.path().join(DATABASE_FILE)).unwrap(),
                before
            );
            connection
                .execute("UPDATE events SET event_version=1 WHERE kind=?1", [kind])
                .unwrap();
        }
    }
    let cold = open_connection(&root, true).unwrap();
    let (_, view) = resolve_view(&cold, f.session, 0, &mut 0).unwrap();
    assert_eq!(
        view.unwrap().rewinds.operations[0].outcome(),
        crate::session::rewind::RewindOutcome::Restored
    );
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
            "INSERT INTO events (sequence, session_id, kind, event_version, payload, created_at_ms)
             VALUES ((SELECT COALESCE(MAX(sequence), 0) + 1 FROM events), ?1, ?2, ?3, ?4, 0)",
            params![
                session_id.to_string(),
                start.kind(),
                start.version(),
                start_payload
            ],
        )
        .expect("first Event");
    let rename = Event::SessionRenamed { title: "x".into() };
    let payload = rename.payload().expect("bounded Event payload");
    let transaction = connection.transaction().expect("seed transaction");
    for _ in 0..renames {
        transaction
            .execute(
                "INSERT INTO events (sequence, session_id, kind, event_version, payload, created_at_ms)
                 VALUES ((SELECT MAX(sequence) + 1 FROM events), ?1, ?2, ?3, ?4, 0)",
                params![session_id.to_string(), rename.kind(), rename.version(), payload],
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
        prepare_fork(&mut connection, f.session, Some(sequence))
            .unwrap()
            .source_session_id,
        f.session
    );
    connection
        .execute(
            "INSERT INTO events
         SELECT (SELECT MAX(sequence) + 1 FROM events), ?1, run_id, agent_run_id, kind,
                event_version, payload, created_at_ms
         FROM events WHERE run_id=?2 ORDER BY sequence LIMIT 1",
            params![SessionId::new().to_string(), run.id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        prepare_fork(&mut connection, f.session, Some(sequence)),
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
    let boundary = prepare_fork(&mut connection, f.session, Some(sequence)).unwrap();
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
        fork_at(&mut connection, boundary, "Refused".into()),
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
        transaction.execute("INSERT INTO events (sequence, session_id, kind, event_version, payload, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![next as i64, f.session.to_string(), rename.kind(), rename.version(), rename.payload().unwrap()]).unwrap();
    }
    transaction.commit().unwrap();
    assert_eq!(
        prepare_fork(&mut connection, f.session, Some(sequence)).unwrap(),
        boundary,
        "exactly one fork Event fits"
    );
    append(&mut connection, &mut hot, f.session, rename).unwrap();
    assert!(matches!(
        prepare_fork(&mut connection, f.session, Some(sequence)),
        Err(StoreError::ReplayLimit)
    ));
    assert!(matches!(
        fork_at(&mut connection, boundary, "Refused".into()),
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
    let boundary =
        prepare_fork(&mut connection, fixture.session, Some(boundary_sequence)).expect("boundary");
    let child = fork_at(&mut connection, boundary, "Earlier".into()).expect("earlier fork");
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

#[tokio::test]
async fn check_evidence_requires_atomic_publication_and_the_latest_successful_attempt() {
    use crate::journal::check::{
        CheckDisposition, CheckEvent, CheckJournal,
        tests::{fixture, start_fixture},
    };
    use crate::provider::evidence::ProviderEvidenceRecord;
    use crate::store::Store;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let mut journal = CheckJournal::start(StateRoot::admit(&path).unwrap(), start_fixture())
        .await
        .unwrap();
    let id = journal.view().id;
    for event in &fixture().1[2..8] {
        journal.append(event.clone()).await.unwrap();
    }
    let now = crate::journal::check::now_ms().unwrap();
    let record = ProviderEvidenceRecord {
        name: "local".into(),
        digest: [1; 32],
        addresses: vec!["127.0.0.1:9321".parse().unwrap()],
        checked_at_ms: now,
        expires_at_ms: now + 60_000,
    };
    let digest = crate::store::check::provider_digest(&record).unwrap();
    journal
        .evidence_started(digest, record.digest)
        .await
        .unwrap();
    journal.publish_provider(record.clone()).await.unwrap();
    let view = journal.close().await.unwrap();
    assert_eq!(view.disposition, Some(CheckDisposition::Succeeded));
    let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
    assert_eq!(store.load_check(id).await.unwrap(), Some(view));
    assert_eq!(
        store.load_provider_evidence("local".into()).await.unwrap(),
        Some(record.clone())
    );
    store.close().await.unwrap();

    let mut next = CheckJournal::start(StateRoot::open_existing(&path).unwrap(), start_fixture())
        .await
        .unwrap();
    let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM provider_evidence", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    // A resurrected cache row cannot overrule the newer attempt.
    connection.execute("INSERT INTO provider_evidence VALUES ('local', ?1, '[\"127.0.0.1:9321\"]', ?2, ?3, ?4)", params![record.digest.as_slice(), record.checked_at_ms, record.expires_at_ms, id.to_string()]).unwrap();
    drop(connection);
    let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
    assert!(
        store
            .load_provider_evidence("local".into())
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
    next.finish(CheckDisposition::Cancelled).await.unwrap();
    next.close().await.unwrap();

    for forged in [
        CheckEvent::ProviderCheckEvidenceCleared,
        CheckEvent::ProviderCheckFinished {
            disposition: CheckDisposition::Succeeded,
            evidence: Some(digest),
        },
    ] {
        let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
        connection.execute("UPDATE provider_check_events SET kind=?1, event_version=?2, payload=?3 WHERE sequence=(SELECT MAX(sequence) FROM provider_check_events)", params![forged.kind(), forged.version(), serde_json::to_string(&forged).unwrap()]).unwrap();
        drop(connection);
        assert!(matches!(
            Store::open_read_only(StateRoot::open_existing(&path).unwrap()),
            Err(StoreError::InvalidHistory)
        ));
    }
}

#[tokio::test]
async fn check_capacity_and_refused_appends_preserve_the_committed_prefix() {
    use crate::journal::check::{
        CheckDisposition, CheckEvent, CheckId, CheckJournal, CheckOp, CheckOpenError, MAX_CHECKS,
        tests::start_fixture,
    };
    use crate::store::Store;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let mut journal = CheckJournal::start(StateRoot::admit(&path).unwrap(), start_fixture())
        .await
        .unwrap();
    let id = journal.view().id;
    let before = journal.view().clone();
    assert!(
        journal
            .append(CheckEvent::ProviderCheckEvidenceStarted {
                digest: [3; 32],
                snapshot: [1; 32]
            })
            .await
            .is_err()
    );
    assert!(journal.receipt().disposition.is_none());
    assert!(
        journal
            .call_started(crate::CheckCallKind::Answer)
            .await
            .is_err()
    );
    drop(journal);
    let store = Store::open(StateRoot::open_existing(&path).unwrap()).unwrap();
    assert_eq!(store.load_check(id).await.unwrap(), Some(before));
    store.check(CheckOp::Recover).await.unwrap();
    let now = crate::journal::check::now_ms().unwrap();
    let retained = crate::provider::evidence::ProviderEvidenceRecord {
        name: "retained".into(),
        digest: [7; 32],
        addresses: vec!["127.0.0.1:9321".parse().unwrap()],
        checked_at_ms: now,
        expires_at_ms: now + 60_000,
    };
    store
        .record_provider_evidence(retained.clone())
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
    let transaction = connection.transaction().unwrap();
    let start = start_fixture();
    let key = start.target.key().unwrap();
    let events = [
        CheckEvent::ProviderCheckStarted { start },
        CheckEvent::ProviderCheckFinished {
            disposition: CheckDisposition::Failed,
            evidence: None,
        },
    ];
    for _ in 2..MAX_CHECKS {
        let id = CheckId::new().to_string();
        for event in &events {
            transaction.execute("INSERT INTO provider_check_events (check_id,target_key,kind,event_version,payload,created_at_ms) VALUES (?1,?2,?3,?4,?5,1)", params![id, key.as_slice(), event.kind(), event.version(), serde_json::to_string(event).unwrap()]).unwrap();
        }
    }
    transaction.commit().unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_check_events", [], |row| {
            row.get(0)
        })
        .unwrap();
    let mut invalid = start_fixture();
    invalid.cost_consent = false;
    assert!(matches!(
        CheckJournal::start(StateRoot::open_existing(&path).unwrap(), invalid).await,
        Err(CheckOpenError {
            source: StoreError::InvalidHistory,
            receipt: None,
        })
    ));
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM provider_check_events", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        count,
        "refused Start rolls back pruning"
    );
    for _ in 0..2 {
        let mut next =
            CheckJournal::start(StateRoot::open_existing(&path).unwrap(), start_fixture())
                .await
                .unwrap();
        assert_eq!(next.view().completed_calls, 0);
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(DISTINCT check_id) FROM provider_check_events",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            MAX_CHECKS as i64
        );
        next.finish(CheckDisposition::Cancelled).await.unwrap();
        next.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
        assert!(
            store.load_check(id).await.unwrap().is_none(),
            "oldest unreferenced check was pruned"
        );
        assert_eq!(
            store
                .load_provider_evidence("retained".into())
                .await
                .unwrap(),
            Some(retained.clone()),
            "referenced proof survives pruning and reopen"
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn a_new_native_attempt_invalidates_prior_credentials_for_the_same_target() {
    use crate::journal::check::{CheckDisposition, CheckJournal};
    use crate::provider::evidence::NativeEvidenceRecord;
    use crate::store::Store;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let store = Store::open(StateRoot::admit(&path).unwrap()).unwrap();
    let now = crate::journal::check::now_ms().unwrap();
    let record = NativeEvidenceRecord {
        fingerprint: [7; 32],
        checked_at_ms: now,
        expires_at_ms: now + 60_000,
    };
    store.record_native_evidence(record.clone()).await.unwrap();
    let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
    let id: String = connection
        .query_row("SELECT check_id FROM native_provider_evidence", [], |row| {
            row.get(0)
        })
        .unwrap();
    let mut start = store
        .load_check(id.parse().unwrap())
        .await
        .unwrap()
        .unwrap()
        .start;
    start.fingerprint = [8; 32];
    store.close().await.unwrap();
    let mut next = CheckJournal::start(StateRoot::open_existing(&path).unwrap(), start)
        .await
        .unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM native_provider_evidence", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
    next.finish(CheckDisposition::Failed).await.unwrap();
    next.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
    assert!(
        store
            .load_native_evidence(record.fingerprint)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
}
