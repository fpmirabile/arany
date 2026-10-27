use super::*;

#[test]
fn selective_editor_steps_preserve_null_records_and_refuse_private_work() {
    for (kind, from, source, field) in [
        (
            "AgentSpawned",
            15,
            include_str!("agentspawned-v15.json"),
            "assignment",
        ),
        (
            "AgentFinished",
            10,
            include_str!("agentfinished-v10.json"),
            "worker",
        ),
    ] {
        assert_eq!(from + 1, crate::test_support::payload_version(kind));
        let source = source.trim();
        let step = REGISTRY.event_step(kind, from).unwrap();
        assert_eq!((step.canonical)(source).as_deref(), Some(source));
        let value: Value = serde_json::from_str(source).unwrap();
        assert_eq!((step.transform)(value.clone()), Some(value.clone()));
        for replacement in [serde_json::json!({}), serde_json::json!({"private":true})] {
            let mut invalid = value.clone();
            invalid[field] = replacement;
            assert!((step.transform)(invalid).is_none());
        }
        let mut invalid = value.clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert!((step.transform)(invalid).is_none());
        let mut invalid = value;
        invalid["unknown"] = Value::Bool(true);
        assert!((step.transform)(invalid).is_none());
    }
    assert_eq!(REGISTRY.event_floor("AgentSpawned"), Some(11));
    assert_eq!(REGISTRY.event_floor("AgentFinished"), Some(10));
    assert_eq!(
        REGISTRY.event_floor("WorkerLeaseReleased"),
        Some(crate::test_support::payload_version("WorkerLeaseReleased"))
    );
    assert!(REGISTRY.event_step("WorkerLeaseReleased", 1).is_none());
}

#[test]
fn private_worker_source_archives_whole_history_and_reopens_without_reinterpretation() {
    for (kind, version, payload) in [
        ("AgentSpawned", 15, include_str!("worker-protocol-archived.json").to_owned()),
        ("AgentFinished", 10, r#"{"worker":{"cleanup":null,"disposition":"ready","cause":"finished","data":null},"disposition":"finished","summary":"done","result":"done"}"#.to_owned()),
        ("WorkerLeaseReleased", 1, r#"{"assignment_id":"01900000-0000-7000-8000-000000000002","handover_sequence":null,"reason":"no_change","cause_sequence":1}"#.to_owned()),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, .. } = forked_history(&path);
        let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
        unsupported_tool_row(&connection, kind, version, &payload);
        drop(connection);
        let original = std::fs::read(path.join(DATABASE_FILE)).unwrap();
        let archived = root.upgrade_or_archive_history(&REGISTRY).unwrap().expect("private protocol archives");
        assert_eq!(std::fs::read(archived.path().join(DATABASE_FILE)).unwrap(), original, "{kind}");
        archived.acknowledge().unwrap();
        let connection = open_connection(&root, false).unwrap();
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        drop(connection);
        assert!(root.upgrade_or_archive_history(&REGISTRY).unwrap().is_none());
    }
}
