use super::*;

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
            workspace_identity: None,
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
            workspace_identity: None,
        },
    )
    .expect("first Event");
    assert!(hot_session.is_some());

    let second = Connection::open(root.path().join(DATABASE_FILE)).expect("second connection");
    let other_id = SessionId::new();
    let other_start = Event::SessionStarted {
        title: "Other".into(),
        workspace_identity: None,
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
            workspace_identity: None,
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
