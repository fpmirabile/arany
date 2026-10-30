use super::*;
use crate::session::SessionId;
use crate::store::RecordKind;
use crate::store::journal::{append, fork_session, open_connection};
use crate::store::replay::resolve_view;
use crate::store::state::FAIL_AT;
use crate::test_support::private_file;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const CHECKPOINTS: &str = include_str!("checkpoints.sql");

struct History {
    root: StateRoot,
    sessions: [SessionId; 2],
}

fn history(path: &Path) -> History {
    let root = StateRoot::admit(path).unwrap();
    let fixture = crate::session::worker_protocol_fixture();
    let mut connection = open_connection(&root, false).unwrap();
    let mut hot = None;
    for event in fixture.events {
        append(&mut connection, &mut hot, fixture.session, event).unwrap();
    }
    let child = fork_session(&mut connection, fixture.session, "Child".into()).unwrap();
    History {
        root,
        sessions: [fixture.session, child],
    }
}

fn checkpoint_fixture(root: &StateRoot) {
    let path = root.path().join(checkpoints::FILES[0]);
    private_file(&path, b"");
    Connection::open(path)
        .unwrap()
        .execute_batch(CHECKPOINTS)
        .unwrap();
}

fn fail_after(steps: Option<usize>) {
    FAIL_AT.with(|fail| fail.set(steps));
}

fn event_count(root: &StateRoot) -> i64 {
    open_connection(root, false)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
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
fn history_at_any_other_version_is_archived_intact_and_replaced_by_a_fresh_store() {
    type Case = (&'static str, fn(&Connection));
    let cases: [Case; 3] = [
        ("removed Event kind", |connection| {
            connection
                .execute_batch(
                    "UPDATE events SET kind = 'ToolPermissionDecided' WHERE sequence = 1",
                )
                .unwrap();
        }),
        ("older schema", |connection| {
            connection
                .pragma_update(None, "user_version", SCHEMA_VERSION - 1)
                .unwrap();
        }),
        ("newer schema", |connection| {
            connection
                .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }),
    ];
    for (name, prepare) in cases {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, .. } = history(&path);
        prepare(&Connection::open(path.join(DATABASE_FILE)).unwrap());
        let original = std::fs::read(path.join(DATABASE_FILE)).unwrap();

        let archived = root.archive_incompatible_history().unwrap().unwrap();

        assert_eq!(
            std::fs::read(archived.path().join(DATABASE_FILE)).unwrap(),
            original,
            "{name}"
        );
        archived.acknowledge().unwrap();
        assert_eq!(event_count(&root), 0, "{name}");
        assert!(root.archive_incompatible_history().unwrap().is_none());
    }
}

#[test]
fn current_history_and_checkpoints_are_never_touched() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = history(&path);
    checkpoint_fixture(&root);
    let journal = std::fs::read(path.join(DATABASE_FILE)).unwrap();
    let original = std::fs::read(path.join(checkpoints::FILES[0])).unwrap();
    for _ in 0..2 {
        assert!(root.archive_incompatible_history().unwrap().is_none());
        assert_eq!(std::fs::read(path.join(DATABASE_FILE)).unwrap(), journal);
        assert_eq!(
            std::fs::read(path.join(checkpoints::FILES[0])).unwrap(),
            original
        );
        assert!(!path.join("archive").exists());
        replays(&root, &sessions);
    }
}

#[test]
fn unsupported_history_in_an_uncheckpointed_wal_is_archived_byte_for_byte() {
    for file in [DATABASE_FILE, checkpoints::FILES[0]] {
        let temp = tempfile::tempdir().unwrap();
        let built = temp.path().join("built");
        let path = temp.path().join("state");
        let History { root: source, .. } = history(&built);
        checkpoint_fixture(&source);
        let writer = Connection::open(built.join(file)).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        if file == DATABASE_FILE {
            writer
                .execute_batch(
                    "UPDATE events SET kind = 'ToolPermissionDecided' WHERE sequence = 1",
                )
                .unwrap();
        } else {
            writer.pragma_update(None, "user_version", 0).unwrap();
        }
        let History { root, .. } = history(&path);
        let mut copied = Vec::new();
        for name in [file.to_owned(), format!("{file}-wal")] {
            std::fs::copy(built.join(&name), path.join(&name)).unwrap();
            std::fs::set_permissions(path.join(&name), std::fs::Permissions::from_mode(0o600))
                .unwrap();
            copied.push((name.clone(), std::fs::read(path.join(&name)).unwrap()));
        }
        drop(writer);

        let archived = root.archive_incompatible_history().unwrap().unwrap();

        for (name, bytes) in copied {
            assert_eq!(
                std::fs::read(archived.path().join(&name)).unwrap(),
                bytes,
                "{name} is archived unchanged"
            );
        }
    }
}

#[test]
fn checkpoints_at_another_version_archive_history_intact_under_existing_locks() {
    for version in [0, checkpoints::SCHEMA_VERSION + 1] {
        let mut completed = false;
        for interruption in 0..64 {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("state");
            let History { root, .. } = history(&path);
            checkpoint_fixture(&root);
            Connection::open(path.join(checkpoints::FILES[0]))
                .unwrap()
                .pragma_update(None, "user_version", version)
                .unwrap();
            let originals: Vec<_> = [DATABASE_FILE, checkpoints::FILES[0]]
                .into_iter()
                .map(|name| (name, std::fs::read(path.join(name)).unwrap()))
                .collect();
            if interruption == 0 {
                let lease = root.lock_history(false).unwrap();
                assert!(matches!(
                    root.archive_incompatible_history(),
                    Err(StoreError::HistoryBusy)
                ));
                drop(lease);
                let lock = checkpoints::lock(&root).unwrap();
                assert!(matches!(
                    root.archive_incompatible_history(),
                    Err(StoreError::CheckpointUnavailable)
                ));
                drop(lock);
            }
            fail_after(Some(interruption));
            let result = root.archive_incompatible_history();
            fail_after(None);
            let archived = match result {
                Ok(archived) => {
                    completed = true;
                    archived.unwrap()
                }
                Err(StoreError::Io(_)) => root.archive_incompatible_history().unwrap().unwrap(),
                Err(error) => panic!("step {interruption}: {error}"),
            };
            for (name, original) in originals {
                assert_eq!(std::fs::read(archived.path().join(name)).unwrap(), original);
            }
            assert!(!path.join(checkpoints::FILES[0]).exists());
            assert_eq!(
                root.archive_incompatible_history().unwrap().unwrap().path(),
                archived.path()
            );
            archived.acknowledge().unwrap();
            assert!(root.archive_incompatible_history().unwrap().is_none());
            assert_eq!(event_count(&root), 0);
            if completed {
                break;
            }
        }
        assert!(
            completed,
            "archive must complete within the failpoint bound"
        );
    }
}

fn hot_checkpoint_journal(
    temp: &Path,
    database: &Path,
    journal: &Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let scratch = temp.join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let writer = Connection::open(database).unwrap();
    writer.execute_batch("PRAGMA cache_size=1; BEGIN;").unwrap();
    writer.execute_batch("CREATE TABLE junk(x BLOB);").unwrap();
    for _ in 0..300 {
        writer
            .execute("INSERT INTO junk VALUES (randomblob(4000))", [])
            .unwrap();
    }
    std::fs::copy(database, scratch.join("db")).unwrap();
    std::fs::copy(journal, scratch.join("journal")).unwrap();
    drop(writer);
    (scratch.join("db"), scratch.join("journal"))
}

#[test]
fn checkpoint_hot_journal_recovers_before_eligibility() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = history(&path);
    checkpoint_fixture(&root);
    let database = path.join(checkpoints::FILES[0]);
    let journal_path = path.join(checkpoints::FILES[1]);
    let original = std::fs::read(&database).unwrap();
    let journal = std::fs::read(path.join(DATABASE_FILE)).unwrap();
    let (db, hot) = hot_checkpoint_journal(temp.path(), &database, &journal_path);
    std::fs::copy(db, &database).unwrap();
    std::fs::copy(hot, &journal_path).unwrap();
    assert_ne!(std::fs::read(&database).unwrap(), original);
    assert!(!checkpoint_incompatible(&root).unwrap());
    assert!(!journal_path.exists());
    assert_eq!(std::fs::read(&database).unwrap(), original);
    assert!(root.archive_incompatible_history().unwrap().is_none());
    assert_eq!(std::fs::read(path.join(DATABASE_FILE)).unwrap(), journal);
    assert!(!path.join("archive").exists());
    replays(&root, &sessions);
}

#[test]
fn a_checkpoint_journal_that_survives_recovery_refuses_untouched() {
    for case in ["regular", "symlink", "orphan"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, sessions } = history(&path);
        if case != "orphan" {
            checkpoint_fixture(&root);
        }
        let journal_path = path.join(checkpoints::FILES[1]);
        let target = path.join("elsewhere");
        match case {
            "symlink" => {
                std::fs::write(&target, vec![0xA5; 512]).unwrap();
                std::os::unix::fs::symlink(&target, &journal_path).unwrap();
            }
            "orphan" => private_file(&journal_path, b"orphan"),
            _ => private_file(&journal_path, b""),
        }
        let database = std::fs::read(path.join(checkpoints::FILES[0])).ok();
        let journal = std::fs::read(path.join(DATABASE_FILE)).unwrap();

        assert!(root.archive_incompatible_history().is_err(), "{case}");

        assert!(journal_path.symlink_metadata().is_ok(), "{case}");
        assert_eq!(
            std::fs::read(path.join(checkpoints::FILES[0])).ok(),
            database,
            "{case}"
        );
        assert_eq!(std::fs::read(path.join(DATABASE_FILE)).unwrap(), journal);
        assert!(!path.join("archive").exists(), "{case}");
        if case == "symlink" {
            assert_eq!(std::fs::read(&target).unwrap(), vec![0xA5; 512]);
        }
        replays(&root, &sessions);
    }
}

#[test]
fn damaged_busy_or_unsafe_checkpoints_are_never_archived() {
    for case in ["malformed", "extra_trigger", "busy", "unsafe"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, sessions } = history(&path);
        checkpoint_fixture(&root);
        let connection = Connection::open(path.join(checkpoints::FILES[0])).unwrap();
        match case {
            "extra_trigger" => connection
                .execute_batch(
                    "CREATE TRIGGER forged AFTER INSERT ON refs BEGIN DELETE FROM blobs; END;",
                )
                .unwrap(),
            "busy" => connection.execute_batch("BEGIN EXCLUSIVE;").unwrap(),
            "malformed" => std::fs::write(path.join(checkpoints::FILES[0]), b"malformed").unwrap(),
            _ => std::os::unix::fs::symlink(
                path.join(checkpoints::FILES[0]),
                path.join(checkpoints::FILES[1]),
            )
            .unwrap(),
        }
        let original = std::fs::read(path.join(checkpoints::FILES[0])).unwrap();
        let journal = std::fs::read(path.join(DATABASE_FILE)).unwrap();
        let result = root.archive_incompatible_history();
        match case {
            "busy" => assert!(
                matches!(result, Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(code, _)))
                if code.code == rusqlite::ErrorCode::DatabaseBusy)
            ),
            "unsafe" => assert!(result.is_err()),
            _ => assert!(result.unwrap().is_none(), "{case}"),
        }
        if case == "extra_trigger" {
            assert!(matches!(
                checkpoints::load(&root, [0; 32], &mut None),
                Err(StoreError::CheckpointUnavailable)
            ));
        }
        assert_eq!(
            std::fs::read(path.join(checkpoints::FILES[0])).unwrap(),
            original,
            "{case}"
        );
        assert_eq!(std::fs::read(path.join(DATABASE_FILE)).unwrap(), journal);
        assert!(!path.join("archive").exists(), "{case}");
        replays(&root, &sessions);
    }
}

// Accepts exactly the closed record `{"ok":true}`, like a strict owner decoder.
const fn format(kind: RecordKind) -> RecordFormat {
    RecordFormat {
        kind,
        accepts: |bytes| bytes == br#"{"ok":true}"#,
    }
}

#[test]
fn refused_strict_records_are_archived_intact_and_accepted_ones_stay_in_place() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let root = StateRoot::admit(&path).unwrap();
    let unknown = format(RecordKind::ToolsConfig);
    let malformed = format(RecordKind::ProviderRouting);
    let accepted = format(RecordKind::ProviderRoster);
    let missing = format(RecordKind::CommandTools);
    let records: [(&RecordFormat, Vec<u8>); 3] = [
        (&unknown, br#"{"ok":true,"canary":"newer"}"#.to_vec()),
        (&malformed, br#"{"ok":tr"#.to_vec()),
        (&accepted, br#"{"ok":true}"#.to_vec()),
    ];
    for (format, bytes) in &records {
        private_file(&path.join(format.kind.file()), bytes);
    }
    let formats = [&unknown, &malformed, &accepted, &missing];

    let archived = root
        .archive_incompatible_records(&formats)
        .unwrap()
        .unwrap();

    assert_eq!(archived.path().parent().unwrap(), path.join("archive"));
    assert!(
        archived
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("records-")
    );
    assert_eq!(
        archived.files(),
        [unknown.kind.file(), malformed.kind.file()].as_slice()
    );
    assert_eq!(
        std::fs::metadata(archived.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for (format, bytes) in &records {
        let file = format.kind.file();
        let moved = archived.files().contains(&file);
        let kept = if moved {
            archived.path().join(file)
        } else {
            path.join(file)
        };
        assert_eq!(&std::fs::read(kept).unwrap(), bytes, "{file}");
        assert_eq!(path.join(file).exists(), !moved, "{file}");
    }
    assert!(
        root.archive_incompatible_records(&formats)
            .unwrap()
            .is_none()
    );
}

#[test]
fn an_interrupted_record_archive_keeps_the_record_intact_in_one_place() {
    let older = format(RecordKind::ProviderRouting);
    let bytes = br#"{"ok":false}"#;
    for fail in 0..5 {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let root = StateRoot::admit(&path).unwrap();
        let source = path.join(older.kind.file());
        private_file(&source, bytes);
        fail_after(Some(fail));
        let result = root.archive_incompatible_records(&[&older]);
        fail_after(None);
        assert!(result.is_err(), "step {fail}");
        let mut copies: Vec<_> = std::fs::read_dir(path.join("archive"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join(older.kind.file()))
            .filter(|file| file.exists())
            .collect();
        if source.exists() {
            copies.push(source.clone());
        }
        assert_eq!(copies.len(), 1, "step {fail}");
        assert_eq!(std::fs::read(&copies[0]).unwrap(), bytes);
        root.archive_incompatible_records(&[&older]).unwrap();
        assert!(!source.exists(), "step {fail}");
    }
}

#[test]
fn inactive_host_archive_preserves_the_source_fixture_and_refuses_interrupted_moves() {
    let source = br#"{"commands":[{"workspace":{"path":"/workspace","device":1,"inode":2},"program":"printf","args":["inspected"],"cwd":"","created_unix_ms":1}]}"#;
    assert!((crate::tools::unguarded_rules::UNGUARDED_COMMANDS.accepts)(
        source
    ));
    let mut interrupted = 0;
    let mut completed = 0;
    let mut manifest_failure = false;
    for fail in 0..16 {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).unwrap();
        private_file(&path.join(RecordKind::UnguardedCommands.file()), source);
        FAIL_AT.with(|point| point.set(Some(fail)));
        let result = state.archive_inactive_host_rules();
        FAIL_AT.with(|point| point.set(None));
        let original = path.join("unguarded-commands.json");
        let target = path.join("archive/inactive-host-commands/unguarded-commands.json");
        let pending = target.with_file_name("pending.json");
        let temporary = target.with_file_name("pending.tmp");
        let manifest = target.with_file_name("manifest.json");
        if fail == 0 {
            assert!(result.is_err());
            assert!(path.join("archive").is_dir());
            assert!(!target.parent().unwrap().exists());
            assert!(
                original.exists(),
                "StateRoot sync precedes preparation and move"
            );
        }
        let retained = if original.exists() {
            &original
        } else {
            &target
        };
        assert_eq!(
            std::fs::read(retained).unwrap(),
            source,
            "failure step {fail}"
        );
        drop(state);
        let reopened = StateRoot::open_existing(&path).unwrap();
        match result {
            Ok(true) => {
                completed += 1;
                assert!(!original.exists());
                assert!(manifest.exists() && !pending.exists() && !temporary.exists());
                assert!(reopened.archive_inactive_host_rules().unwrap());
            }
            Err(_) => {
                interrupted += 1;
                if manifest.exists() {
                    manifest_failure = true;
                    assert!(
                        pending.exists(),
                        "manifest sync failure retains the interlock"
                    );
                }
                if pending.exists() || temporary.exists() || target.exists() || manifest.exists() {
                    assert!(
                        matches!(
                            reopened.archive_inactive_host_rules(),
                            Err(StoreError::ReplaceUnconfirmed)
                        ),
                        "no retry or automatic completion of unconfirmed archive at step {fail}"
                    );
                    assert_eq!(std::fs::read(retained).unwrap(), source);
                }
            }
            Ok(false) => panic!("present source was not archived"),
        }
        assert!(!path.join("permissions.json").exists());
    }
    assert!(completed > 0 && interrupted > 0 && manifest_failure);
}
