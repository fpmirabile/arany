use super::*;
use crate::session::{AgentRunId, PAYLOAD_VERSIONS, RunId};
use crate::store::journal::{append, fork_session, open_connection};
use crate::store::state::FAIL_AT;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

// The frozen schema 4 that the 4 -> 5 step upgrades.
const SCHEMA_4_SQL: &str = "CREATE TABLE events (
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
    ) STRICT;";
const SCHEMA_FIXTURES: [(i64, &str); 1] = [(4, SCHEMA_4_SQL)];

type EventFixture<'a> = (&'a str, i64, &'a str, &'a str);
// Production registers no Event step yet; each future step adds its (kind, from, input, expected) row.
const EVENT_FIXTURES: [EventFixture<'static>; 0] = [];

// Synthetic test-only edge: a version-5 SessionStarted stored `name` where version 6 stores `title`.
fn started_v5(value: Value) -> Option<Value> {
    let Value::Object(mut fields) = value else {
        return None;
    };
    let title = fields.remove("name")?;
    fields.insert("title".into(), title);
    (fields.len() == 3).then_some(Value::Object(fields))
}
const STARTED_STEP: [EventStep; 1] = [EventStep {
    kind: "SessionStarted",
    from: 5,
    transform: started_v5,
}];
const REFUSING_STEP: [EventStep; 1] = [EventStep {
    kind: "SessionStarted",
    from: 5,
    transform: |_| None,
}];
const STARTED_FIXTURE: EventFixture<'static> = (
    "SessionStarted",
    5,
    r#"{"name":"Session","workspace_device":1,"workspace_inode":2}"#,
    r#"{"title":"Session","workspace_device":1,"workspace_inode":2}"#,
);

fn started_floor() -> Vec<(&'static str, i64)> {
    EVENT_FLOOR
        .iter()
        .map(|&(kind, floor)| (kind, if kind == "SessionStarted" { 5 } else { floor }))
        .collect()
}

fn synthetic<'a>(floor: &'a [(&'a str, i64)], steps: &'a [EventStep]) -> Registry<'a> {
    Registry {
        event_floor: floor,
        event_steps: steps,
        ..REGISTRY
    }
}

fn fail_after(steps: Option<usize>) {
    FAIL_AT.with(|fail| fail.set(steps));
}

struct History {
    root: StateRoot,
    sessions: [SessionId; 3],
}

// A source Session with a finished Run, a child forked from it and a grandchild forked from the child.
fn forked_history(path: &Path) -> History {
    let root = StateRoot::admit(path).unwrap();
    let fixture = crate::session::worker_protocol_fixture();
    let mut connection = open_connection(&root, false).unwrap();
    let mut hot = None;
    for event in &fixture.events {
        append(&mut connection, &mut hot, fixture.session, event.clone()).unwrap();
    }
    let child = fork_session(&mut connection, fixture.session, "Child".into()).unwrap();
    let grandchild = fork_session(&mut connection, child, "Grandchild".into()).unwrap();
    History {
        root,
        sessions: [fixture.session, child, grandchild],
    }
}

fn rows(connection: &Connection) -> Vec<RawRow> {
    let mut statement = connection.prepare(ROWS_AFTER_SQL).unwrap();
    let mut rows: Vec<RawRow> = Vec::new();
    loop {
        let after = rows.last().map_or(0, |row| row.sequence);
        let chunk = statement
            .query_map([after], |row| Ok(raw_row(row).unwrap()))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        if chunk.is_empty() {
            return rows;
        }
        rows.extend(chunk);
    }
}

// Rewrites SessionStarted to the synthetic version 5 and re-pins forks over the stored bytes, as that build wrote them.
fn downgrade_started(connection: &Connection) {
    let mut hashers = HashMap::<String, PrefixHasher>::new();
    let mut boundaries = HashMap::<(String, i64), [u8; 32]>::new();
    for mut row in rows(connection) {
        if row.kind == "SessionStarted" {
            let mut fields: serde_json::Map<String, Value> =
                serde_json::from_str(&row.payload).unwrap();
            let title = fields.remove("title").unwrap();
            fields.insert("name".into(), title);
            row.payload = Value::Object(fields).to_string();
            row.version = 5;
        }
        if row.kind == "SessionForked" {
            let mut event =
                Event::decode(&row.kind, row.version, &row.payload, None, None).unwrap();
            let Event::SessionForked {
                source_session_id,
                source_sequence,
                prefix_digest,
                ..
            } = &mut event
            else {
                unreachable!()
            };
            *prefix_digest = boundaries[&(
                source_session_id.to_string(),
                i64::try_from(*source_sequence).unwrap(),
            )];
            row.payload = event.payload().unwrap();
        }
        connection
            .execute(
                "UPDATE events SET event_version=?1, payload=?2 WHERE sequence=?3",
                rusqlite::params![row.version, row.payload, row.sequence],
            )
            .unwrap();
        let hasher = hashers.entry(row.session_id.clone()).or_default();
        hasher
            .update_row(&PrefixRow {
                sequence: u64::try_from(row.sequence).unwrap(),
                session_id: &row.session_id,
                run_id: row.run_id.as_deref(),
                agent_run_id: row.agent_run_id.as_deref(),
                kind: &row.kind,
                payload: &row.payload,
                created_at_ms: row.created_at_ms,
            })
            .unwrap();
        if matches!(row.kind.as_str(), "RunFinished" | "SessionForked") {
            boundaries.insert((row.session_id.clone(), row.sequence), hasher.digest());
        }
    }
}

fn stored_rows(connection: &Connection) -> Vec<(i64, String, i64, String)> {
    rows(connection)
        .into_iter()
        .map(|row| (row.sequence, row.kind, row.version, row.payload))
        .collect()
}

fn fork_digests(connection: &Connection) -> Vec<String> {
    rows(connection)
        .into_iter()
        .filter(|row| row.kind == "SessionForked")
        .map(|row| {
            serde_json::from_str::<Value>(&row.payload).unwrap()["prefix_digest"].to_string()
        })
        .collect()
}

fn entries(path: &Path) -> Vec<String> {
    let mut names = match std::fs::read_dir(path) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

fn user_version(connection: &Connection) -> i64 {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn facts(path: &Path) -> Vec<(String, i64, i64, i64, String)> {
    let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
    connection
        .prepare("SELECT id, from_schema, to_schema, rewritten_events, backup FROM migrations")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn replays(root: &StateRoot, sessions: &[SessionId]) {
    let connection = open_connection(root, false).unwrap();
    for session in sessions {
        let (_, view) = resolve_view(&connection, *session, 0, &mut 0).unwrap();
        assert!(view.is_some());
    }
}

#[test]
fn a_migratable_history_upgrades_in_a_validated_copy_with_recomputed_fork_digests() {
    let temp = tempfile::tempdir().unwrap();
    let built = temp.path().join("built");
    let path = temp.path().join("state");
    let History { sessions, .. } = forked_history(&built);
    let writer = Connection::open(built.join(DATABASE_FILE)).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    downgrade_started(&writer);
    let original = stored_rows(&writer);
    let old_digests = fork_digests(&writer);
    let root = StateRoot::admit(&path).unwrap();
    // A crash leaves the downgrade only in the uncheckpointed WAL.
    for name in [DATABASE_FILE, "events.sqlite3-wal"] {
        std::fs::copy(built.join(name), path.join(name)).unwrap();
        std::fs::set_permissions(path.join(name), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    drop(writer);
    let floor = started_floor();

    assert!(
        root.upgrade_or_archive_history(&synthetic(&floor, &STARTED_STEP))
            .unwrap()
            .is_none()
    );

    assert!(!path.join("archive").exists());
    for sidecar in &HISTORY_FILES[..3] {
        assert!(!path.join(sidecar).exists(), "{sidecar}");
    }
    let migrations = path.join(DIR);
    let names = entries(&migrations);
    assert_eq!(names.len(), 1, "only the backup remains: {names:?}");
    let backup = migrations.join(&names[0]);
    for (entry, mode) in [(&migrations, 0o700), (&backup, 0o600)] {
        assert_eq!(
            std::fs::metadata(entry).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
    let backup = Connection::open_with_flags(&backup, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        stored_rows(&backup),
        original,
        "the backup holds every committed row"
    );
    drop(backup);
    let [fact] = facts(&path).try_into().unwrap();
    assert_eq!(
        (fact.1, fact.2, fact.3, fact.4),
        (SCHEMA_VERSION, SCHEMA_VERSION, 3, names[0].clone()),
        "one SessionStarted and two fork digests rewritten"
    );
    replays(&root, &sessions);
    let upgraded = Connection::open(path.join(DATABASE_FILE)).unwrap();
    let new_digests = fork_digests(&upgraded);
    assert_eq!(new_digests.len(), 2);
    assert!(
        new_digests
            .iter()
            .zip(&old_digests)
            .all(|(new, old)| new != old),
        "every fork digest covers the rewritten prefix"
    );
    let started = stored_rows(&upgraded)
        .into_iter()
        .find(|row| row.1 == "SessionStarted")
        .unwrap();
    assert_eq!(started.2, 6);
    assert!(started.3.starts_with(r#"{"title":"#), "{}", started.3);
}

#[test]
fn every_interrupted_migration_step_ends_original_or_upgraded_and_recovers() {
    let mut interrupted = 0;
    loop {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, sessions } = forked_history(&path);
        Connection::open(path.join(DATABASE_FILE))
            .unwrap()
            .execute_batch("DROP TABLE migrations; PRAGMA user_version=4;")
            .unwrap();
        let original = std::fs::read(path.join(DATABASE_FILE)).unwrap();
        let original_rows = stored_rows(&Connection::open(path.join(DATABASE_FILE)).unwrap());

        fail_after(Some(interrupted));
        let first = root.archive_incompatible_history();
        fail_after(None);
        match first {
            Ok(None) => break,
            Err(StoreError::Io(_)) => {}
            other => panic!("step {interrupted}: {other:?}"),
        }
        let current = std::fs::read(path.join(DATABASE_FILE)).unwrap();
        if current != original {
            assert_eq!(facts(&path).len(), 1, "step {interrupted}");
        }
        if read_marker(&root)
            .unwrap()
            .is_some_and(|(_, marker)| matches!(marker.phase, Phase::Preparing))
        {
            std::fs::write(path.join(DATABASE_FILE), b"newer-history").unwrap();
            assert!(matches!(
                root.archive_incompatible_history(),
                Err(StoreError::MigrationConflict)
            ));
            assert!(pending(&root).unwrap() && !path.join("archive").exists());
            std::fs::write(path.join(DATABASE_FILE), &original).unwrap();
        }

        assert!(
            root.archive_incompatible_history().unwrap().is_none(),
            "step {interrupted}"
        );
        let [fact] = facts(&path).try_into().unwrap();
        assert_eq!((fact.1, fact.2), (4, SCHEMA_VERSION), "step {interrupted}");
        replays(&root, &sessions);
        let backups = entries(&path.join(DIR));
        assert!(
            !backups.is_empty() && backups.iter().all(|name| name.starts_with("history-")),
            "step {interrupted}: {backups:?}"
        );
        for name in backups {
            let backup = Connection::open(path.join(DIR).join(name)).unwrap();
            assert_eq!(user_version(&backup), 4, "step {interrupted}");
            assert_eq!(stored_rows(&backup), original_rows, "step {interrupted}");
        }
        interrupted += 1;
    }
    assert!(interrupted >= 16, "every durability step was interrupted");
}

#[test]
fn unsupported_or_refused_history_is_archived_intact() {
    let floor = started_floor();
    type Case<'a> = (&'a str, Registry<'a>, fn(&Connection));
    let cases: [Case<'_>; 3] = [
        ("schema below the floor", REGISTRY, |connection| {
            connection
                .execute_batch("DROP TABLE migrations; PRAGMA user_version=3;")
                .unwrap();
        }),
        (
            "refused step",
            synthetic(&floor, &REFUSING_STEP),
            downgrade_started,
        ),
        (
            "forged fork digest",
            synthetic(&floor, &STARTED_STEP),
            |connection| {
                downgrade_started(connection);
                let child_fork: String = connection
                .query_row(
                    "SELECT payload FROM events WHERE kind='SessionForked' ORDER BY sequence LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
                let mut event = Event::decode("SessionForked", 6, &child_fork, None, None).unwrap();
                if let Event::SessionForked { prefix_digest, .. } = &mut event {
                    prefix_digest[0] ^= 1;
                }
                connection
                    .execute(
                        "UPDATE events SET payload=?1 WHERE payload=?2",
                        [event.payload().unwrap(), child_fork],
                    )
                    .unwrap();
            },
        ),
    ];
    for (name, registry, prepare) in cases {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, .. } = forked_history(&path);
        prepare(&Connection::open(path.join(DATABASE_FILE)).unwrap());
        let original = std::fs::read(path.join(DATABASE_FILE)).unwrap();

        let archived = root.upgrade_or_archive_history(&registry).unwrap().unwrap();

        assert_eq!(
            std::fs::read(archived.path().join(DATABASE_FILE)).unwrap(),
            original,
            "{name}"
        );
        assert!(entries(&path.join(DIR)).is_empty(), "{name}");
        archived.acknowledge().unwrap();
        let fresh = open_connection(&root, false).unwrap();
        assert_eq!(stored_rows(&fresh), Vec::new(), "{name}");
    }
}

// The guard: every version above its declared floor needs a registered step and fixture per edge.
fn uncovered(
    registry: &Registry<'_>,
    events: &[(&str, i64)],
    schema: i64,
    event_fixtures: &[EventFixture<'_>],
) -> Vec<String> {
    let mut problems = Vec::new();
    for &(kind, current) in events {
        let Some(floor) = registry.event_floor(kind) else {
            problems.push(format!("{kind} declares no migration floor"));
            continue;
        };
        let mut chained = None;
        for from in floor..current {
            let step = registry.event_step(kind, from);
            let fixture = event_fixtures
                .iter()
                .find(|fixture| fixture.0 == kind && fixture.1 == from);
            let (Some(step), Some(&(_, _, input, expected))) = (step, fixture) else {
                problems.push(format!(
                    "{kind} {from} -> {} has no step and fixture",
                    from + 1
                ));
                continue;
            };
            let input: Value = serde_json::from_str(input).unwrap();
            let expected: Value = serde_json::from_str(expected).unwrap();
            if (step.transform)(input.clone()).as_ref() != Some(&expected) {
                problems.push(format!("{kind} {from} -> {} fixture mismatch", from + 1));
            }
            chained = Some(chained.unwrap_or(Some(input)).and_then(step.transform));
        }
        if let Some(value) = chained {
            let scopes = [
                (None, None),
                (Some(RunId::new()), None),
                (Some(RunId::new()), Some(AgentRunId::new())),
            ];
            let decodes = value.is_some_and(|value| {
                scopes.into_iter().any(|(run, agent)| {
                    Event::decode(kind, current, &value.to_string(), run, agent).is_ok()
                })
            });
            if !decodes {
                problems.push(format!("{kind} {floor} -> {current} does not decode"));
            }
        }
    }
    let current_schema = schema_of(
        &crate::store::journal::SCHEMA_SQL
            .replace("BEGIN IMMEDIATE;", "")
            .replace("COMMIT;", ""),
    );
    for from in registry.schema_floor..schema {
        let fixture = SCHEMA_FIXTURES.iter().find(|fixture| fixture.0 == from);
        let (Some(step), Some((_, sql))) = (registry.schema_step(from), fixture) else {
            problems.push(format!(
                "schema {from} -> {} has no step and fixture",
                from + 1
            ));
            continue;
        };
        let target = SCHEMA_FIXTURES
            .iter()
            .find(|fixture| fixture.0 == from + 1)
            .map_or_else(|| current_schema.clone(), |(_, sql)| schema_of(sql));
        if schema_of(&format!("{sql}{}", step.sql)) != target {
            problems.push(format!("schema {from} -> {} fixture mismatch", from + 1));
        }
    }
    problems
}

fn schema_of(sql: &str) -> Vec<(String, String)> {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(sql).unwrap();
    connection
        .prepare("SELECT name, sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |row| {
            let sql: String = row.get(1)?;
            Ok((
                row.get(0)?,
                sql.split_whitespace().collect::<Vec<_>>().join(" "),
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn every_stored_version_above_its_floor_has_a_step_and_fixture() {
    assert_eq!(
        uncovered(
            &REGISTRY,
            &PAYLOAD_VERSIONS,
            SCHEMA_VERSION,
            &EVENT_FIXTURES
        ),
        Vec::<String>::new()
    );
    let floor = started_floor();
    assert_eq!(
        uncovered(
            &synthetic(&floor, &STARTED_STEP),
            &PAYLOAD_VERSIONS,
            SCHEMA_VERSION,
            &[STARTED_FIXTURE]
        ),
        Vec::<String>::new(),
        "a registered step with its fixture covers its edge"
    );

    let mut bumped = PAYLOAD_VERSIONS.to_vec();
    bumped.iter_mut().for_each(|(kind, version)| {
        if *kind == "SessionRenamed" {
            *version += 1;
        }
    });
    bumped.push(("SyntheticKind", 1));
    assert_eq!(
        uncovered(&REGISTRY, &bumped, SCHEMA_VERSION + 1, &EVENT_FIXTURES),
        [
            "SessionRenamed 6 -> 7 has no step and fixture",
            "SyntheticKind declares no migration floor",
            "schema 5 -> 6 has no step and fixture",
        ]
    );
}
