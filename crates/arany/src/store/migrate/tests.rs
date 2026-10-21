use super::*;
use crate::session::{AgentRunId, CompactionRecord, PAYLOAD_VERSIONS, RunId};
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
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StartedV5 {
    name: String,
    workspace_device: u64,
    workspace_inode: u64,
}
fn started_v5(value: Value) -> Option<Value> {
    let Value::Object(mut fields) = value else {
        return None;
    };
    let title = fields.remove("name")?;
    fields.insert("title".into(), title);
    (fields.len() == 3).then_some(Value::Object(fields))
}
fn started_v5_canonical(payload: &str) -> Option<String> {
    serde_json::to_string(&serde_json::from_str::<StartedV5>(payload).ok()?).ok()
}
const STARTED_STEP: [EventStep; 1] = [EventStep {
    kind: "SessionStarted",
    from: 5,
    transform: started_v5,
    canonical: started_v5_canonical,
}];
const REFUSING_STEP: [EventStep; 1] = [EventStep {
    kind: "SessionStarted",
    from: 5,
    transform: |_| None,
    canonical: started_v5_canonical,
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

fn failed_compaction(run: RunId, covered_sequence: u64, digest: [u8; 32]) -> CompactionRecord {
    CompactionRecord {
        covered_run_id: run,
        covered_sequence,
        source_digest: digest,
        provider: "scripted".into(),
        model: "test".into(),
        prompt_version: 1,
        schema_version: 1,
        compiler_version: 3,
        source_bytes: 1,
        source_tokens_estimate: 1,
        input_tokens: None,
        output_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        response_id: None,
        wire_provenance: None,
        chatgpt_provenance: None,
        output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
        status: crate::session::CompactionStatus::Failed {
            reason: crate::session::CompactionFailure::ProviderUnavailable,
        },
    }
}

struct History {
    root: StateRoot,
    sessions: [SessionId; 3],
}

// A source with a finished Run, a rewind and a compaction over it, a child fork and a grandchild fork.
fn forked_history(path: &Path) -> History {
    let root = StateRoot::admit(path).unwrap();
    let fixture = crate::session::worker_protocol_fixture();
    let source = fixture.session;
    let mut connection = open_connection(&root, false).unwrap();
    let mut hot = None;
    for event in &fixture.events {
        append(&mut connection, &mut hot, source, event.clone()).unwrap();
    }
    let view = |connection: &Connection| resolve_view(connection, source, 0, &mut 0).unwrap();
    let current = view(&connection).1.unwrap();
    let started =
        crate::session::rewind_started_fixture(&current, current.rewinds.checkpoints[0].clone());
    append(&mut connection, &mut hot, source, started).unwrap();
    let finished = crate::session::rewind_finished_fixture(
        &view(&connection).1.unwrap(),
        crate::session::rewind::RewindOutcome::Restored,
    );
    append(&mut connection, &mut hot, source, finished).unwrap();
    let (events, current) = view(&connection);
    let covered = current.unwrap().last_sequence;
    let record = failed_compaction(
        fixture.run,
        covered,
        crate::session::prefix_digest(&events, covered).unwrap(),
    );
    let compacted = Event::ContextCompacted {
        record,
        active_ms: None,
    };
    append(&mut connection, &mut hot, source, compacted).unwrap();
    let child = fork_session(&mut connection, source, "Child".into()).unwrap();
    let grandchild = fork_session(&mut connection, child, "Grandchild".into()).unwrap();
    History {
        root,
        sessions: [source, child, grandchild],
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

fn stored_prefix(row: &RawRow) -> PrefixRow<'_> {
    PrefixRow {
        sequence: u64::try_from(row.sequence).unwrap(),
        session_id: &row.session_id,
        run_id: row.run_id.as_deref(),
        agent_run_id: row.agent_run_id.as_deref(),
        kind: &row.kind,
        payload: &row.payload,
        created_at_ms: row.created_at_ms,
    }
}

// Rewrites SessionStarted to the synthetic version 5 and re-pins every prefix reference over the
// stored bytes. With canonical text this is what a version-5 build wrote; noncanonical text pins
// digests that version-5 replay rejected.
fn downgrade_started(connection: &Connection, canonical: bool) {
    let mut lanes = HashMap::<String, PrefixHasher>::new();
    let mut points = HashMap::<(String, u64), [u8; 32]>::new();
    for mut row in rows(connection) {
        if row.kind == "SessionStarted" {
            let current: Value = serde_json::from_str(&row.payload).unwrap();
            let (name, device, inode) = (
                &current["title"],
                &current["workspace_device"],
                &current["workspace_inode"],
            );
            row.payload = if canonical {
                format!(
                    r#"{{"name":{name},"workspace_device":{device},"workspace_inode":{inode}}}"#
                )
            } else {
                format!(
                    r#"{{"workspace_inode":{inode},"name":{name},"workspace_device":{device}}}"#
                )
            };
            row.version = 5;
        }
        let lane = lanes.entry(row.session_id.clone()).or_default();
        if matches!(
            row.kind.as_str(),
            "SessionForked" | "ContextCompacted" | "WorkspaceRewindStarted"
        ) {
            let mut event =
                Event::decode(&row.kind, row.version, &row.payload, None, None).unwrap();
            match &mut event {
                Event::SessionForked {
                    source_session_id,
                    source_sequence,
                    prefix_digest,
                    ..
                } => *prefix_digest = points[&(source_session_id.to_string(), *source_sequence)],
                Event::ContextCompacted { record, .. } => {
                    record.source_digest =
                        points[&(row.session_id.clone(), record.covered_sequence)];
                }
                Event::WorkspaceRewindStarted { record } => record.prefix_digest = lane.digest(),
                _ => unreachable!(),
            }
            row.payload = event.payload().unwrap();
        }
        connection
            .execute(
                "UPDATE events SET event_version=?1, payload=?2 WHERE sequence=?3",
                rusqlite::params![row.version, row.payload, row.sequence],
            )
            .unwrap();
        lane.update_row(&stored_prefix(&row)).unwrap();
        points.insert(
            (row.session_id.clone(), u64::try_from(row.sequence).unwrap()),
            lane.digest(),
        );
    }
}

fn forge(connection: &Connection, kind: &str) {
    let (sequence, payload): (i64, String) = connection
        .query_row(
            "SELECT sequence, payload FROM events WHERE kind=?1 ORDER BY sequence LIMIT 1",
            [kind],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let version = payload_version(kind).unwrap();
    let mut event = Event::decode(kind, version, &payload, None, None).unwrap();
    match &mut event {
        Event::SessionForked { prefix_digest, .. } => prefix_digest[0] ^= 1,
        Event::ContextCompacted { record, .. } => record.source_digest[0] ^= 1,
        Event::WorkspaceRewindStarted { record } => record.prefix_digest[0] ^= 1,
        _ => unreachable!(),
    }
    connection
        .execute(
            "UPDATE events SET payload=?1 WHERE sequence=?2",
            rusqlite::params![event.payload().unwrap(), sequence],
        )
        .unwrap();
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
fn a_migratable_history_upgrades_in_a_validated_copy_with_recomputed_prefix_digests() {
    let temp = tempfile::tempdir().unwrap();
    let built = temp.path().join("built");
    let path = temp.path().join("state");
    let History { sessions, .. } = forked_history(&built);
    let writer = Connection::open(built.join(DATABASE_FILE)).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    downgrade_started(&writer, true);
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
    assert!(
        names[0].ends_with(&format!("-{}.sqlite3", fact.0)),
        "{names:?}"
    );
    assert_eq!(
        (fact.1, fact.2, fact.3, fact.4),
        (SCHEMA_VERSION, SCHEMA_VERSION, 5, names[0].clone()),
        "SessionStarted, the rewind, the compaction and two forks are rewritten"
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
    // One fixed clock makes a retry after a kept backup reuse the same timestamp.
    CLOCK.with(|clock| clock.set(Some(SystemTime::UNIX_EPOCH)));
    let mut interrupted = 0;
    loop {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, sessions } = forked_history(&path);
        Connection::open(path.join(DATABASE_FILE))
            .unwrap()
            .execute_batch("DROP TABLE migrations; PRAGMA user_version=4;")
            .unwrap();
        let database = path.join(DATABASE_FILE);
        let original = std::fs::read(&database).unwrap();
        let original_rows = stored_rows(&Connection::open(&database).unwrap());

        fail_after(Some(interrupted));
        let first = root.archive_incompatible_history();
        fail_after(None);
        match first {
            Ok(None) => break,
            Err(StoreError::Io(_)) => {}
            other => panic!("step {interrupted}: {other:?}"),
        }
        let current = std::fs::read(&database).unwrap();
        if current != original {
            assert_eq!(facts(&path).len(), 1, "step {interrupted}");
        }
        let mut kept_backup = false;
        if let Some((_, marker)) = read_marker(&root).unwrap() {
            // A changed artifact fails closed before any cleanup.
            let before = entries(&path.join(DIR));
            let changed = match marker.phase {
                Phase::Preparing => database.clone(),
                Phase::Ready { .. } => {
                    kept_backup = current == original;
                    path.join(DIR).join(&marker.backup)
                }
            };
            let saved = std::fs::read(&changed).unwrap();
            std::fs::write(&changed, b"changed").unwrap();
            assert!(
                matches!(
                    root.archive_incompatible_history(),
                    Err(StoreError::MigrationConflict)
                ),
                "step {interrupted}"
            );
            assert_eq!(entries(&path.join(DIR)), before, "step {interrupted}");
            assert!(!path.join("archive").exists(), "step {interrupted}");
            std::fs::write(&changed, saved).unwrap();
            assert_eq!(
                std::fs::read(&database).unwrap(),
                current,
                "step {interrupted}"
            );
        }

        assert!(
            root.archive_incompatible_history().unwrap().is_none(),
            "step {interrupted}"
        );
        let [fact] = facts(&path).try_into().unwrap();
        assert_eq!((fact.1, fact.2), (4, SCHEMA_VERSION), "step {interrupted}");
        replays(&root, &sessions);
        let backups = entries(&path.join(DIR));
        assert_eq!(
            backups.len(),
            if kept_backup { 2 } else { 1 },
            "step {interrupted}: {backups:?}"
        );
        for name in backups {
            assert!(name.starts_with("history-19700101T000000.000Z-"), "{name}");
            let backup = Connection::open(path.join(DIR).join(name)).unwrap();
            assert_eq!(user_version(&backup), 4, "step {interrupted}");
            assert_eq!(stored_rows(&backup), original_rows, "step {interrupted}");
        }
        interrupted += 1;
    }
    CLOCK.with(|clock| clock.set(None));
    assert!(interrupted >= 16, "every durability step was interrupted");
}

#[test]
fn unsupported_refused_or_forged_history_is_archived_intact() {
    let floor = started_floor();
    let (refusing, upgrading) = (
        synthetic(&floor, &REFUSING_STEP),
        synthetic(&floor, &STARTED_STEP),
    );
    type Case<'a> = (&'a str, &'a Registry<'a>, fn(&Connection));
    let cases: [Case<'_>; 6] = [
        ("schema below the floor", &REGISTRY, |connection| {
            connection
                .execute_batch("DROP TABLE migrations; PRAGMA user_version=3;")
                .unwrap();
        }),
        ("refused step", &refusing, |connection| {
            downgrade_started(connection, true);
        }),
        (
            "digests pinned over noncanonical bytes",
            &upgrading,
            |connection| {
                downgrade_started(connection, false);
            },
        ),
        ("forged fork digest", &upgrading, |connection| {
            downgrade_started(connection, true);
            forge(connection, "SessionForked");
        }),
        ("forged rewind prefix", &upgrading, |connection| {
            downgrade_started(connection, true);
            forge(connection, "WorkspaceRewindStarted");
        }),
        ("forged compaction digest", &upgrading, |connection| {
            downgrade_started(connection, true);
            forge(connection, "ContextCompacted");
        }),
    ];
    for (name, registry, prepare) in cases {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, .. } = forked_history(&path);
        prepare(&Connection::open(path.join(DATABASE_FILE)).unwrap());
        let original = std::fs::read(path.join(DATABASE_FILE)).unwrap();

        let archived = root.upgrade_or_archive_history(registry).unwrap().unwrap();

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

#[test]
fn goal_compaction_digests_are_verified_before_they_are_repinned() {
    let floor = started_floor();
    let registry = synthetic(&floor, &STARTED_STEP);
    for forged in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let fixture = crate::session::worker_protocol_fixture();
        let mut connection = open_connection(&root, false).unwrap();
        let mut hot = None;
        for event in &fixture.events {
            append(&mut connection, &mut hot, fixture.session, event.clone()).unwrap();
        }
        let rows = rows(&connection);
        let tool = rows.iter().find(|row| row.kind == "ToolFinished").unwrap();
        let mut hasher = PrefixHasher::new();
        hasher.update_row(&stored_prefix(tool)).unwrap();
        let mut record = failed_compaction(
            fixture.run,
            u64::try_from(tool.sequence).unwrap(),
            hasher.digest(),
        );
        record.source_digest[0] ^= u8::from(forged);
        let event = Event::GoalContextCompacted {
            goal_id: fixture.goal,
            run_id: fixture.run,
            covered_tool_count: 1,
            record,
        };
        connection
            .execute(
                "INSERT INTO events (sequence, session_id, run_id, kind, event_version, payload, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                rusqlite::params![
                    rows.last().unwrap().sequence + 1,
                    fixture.session.to_string(),
                    fixture.run.to_string(),
                    event.kind(),
                    event.version(),
                    event.payload().unwrap(),
                ],
            )
            .unwrap();
        downgrade_started(&connection, true);

        let transaction = connection.transaction().unwrap();
        let result = rewrite_events(&transaction, &registry);

        if forged {
            assert!(matches!(result, Err(StoreError::InvalidHistory)));
        } else {
            assert_eq!(result.unwrap(), 1, "only SessionStarted changes");
        }
    }
}

#[test]
fn replay_validation_stops_at_its_cumulative_event_bound() {
    let temp = tempfile::tempdir().unwrap();
    let History { root, .. } = forked_history(&temp.path().join("state"));
    let connection = open_connection(&root, false).unwrap();
    let stored = rows(&connection).len();

    validate_all(&connection, MAX_VALIDATION_EVENTS).unwrap();
    // Forks reload their ancestors, so whole-history replay loads more events than are stored.
    assert!(matches!(
        validate_all(&connection, stored),
        Err(StoreError::ReplayLimit)
    ));
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
            if (step.canonical)(input).as_deref() != Some(input) {
                problems.push(format!("{kind} {from} fixture is not canonical"));
            }
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
