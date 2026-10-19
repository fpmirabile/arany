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

    let second = Connection::open(root.path().join(DATABASE_FILE)).expect("second connection");
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
