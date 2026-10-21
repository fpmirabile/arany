use super::*;
use crate::store::state::FAIL_AT;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const CANARY: &str = "record-canary-7f3a";

// Synthetic test-only edge: version 1 stored `name` where version 2 stores `title`.
fn renamed(value: Value) -> Option<Value> {
    let Value::Object(mut fields) = value else {
        return None;
    };
    let title = fields.remove("name")?;
    fields.insert("title".into(), title);
    fields.insert("version".into(), 2.into());
    Some(Value::Object(fields))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Current {
    version: u8,
    title: String,
}

fn current(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Current>(bytes)
        .is_ok_and(|record| record.version == 2 && !record.title.is_empty())
}

const fn synthetic(kind: RecordKind) -> RecordFormat {
    RecordFormat {
        kind,
        field: "version",
        floor: 1,
        current: 2,
        steps: &[RecordStep {
            from: 1,
            transform: renamed,
        }],
        accepts: current,
    }
}

const FORMAT: RecordFormat = synthetic(RecordKind::UiPreferences);

fn root(temp: &Path) -> (StateRoot, std::path::PathBuf) {
    let path = temp.join("state");
    (StateRoot::admit(&path).unwrap(), path)
}

fn plant(path: &Path, kind: RecordKind, bytes: &[u8]) {
    let file = path.join(kind.file());
    std::fs::write(&file, bytes).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn on_disk(path: &Path, kind: RecordKind) -> (Vec<u8>, bool) {
    (
        std::fs::read(path.join(kind.file())).unwrap(),
        path.join(kind.pending()).exists(),
    )
}

fn sized_v1(bytes: usize) -> Vec<u8> {
    let frame = br#"{"version":1,"name":""}"#.len();
    format!(r#"{{"version":1,"name":"{}"}}"#, "a".repeat(bytes - frame)).into_bytes()
}

#[test]
fn a_behind_record_upgrades_in_its_pending_replacement_and_any_other_stays_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let (state, path) = root(temp.path());
    let kind = FORMAT.kind;
    assert_eq!(state.read_current(&FORMAT).unwrap(), None);

    plant(&path, kind, br#"{"version":1,"name":"Session"}"#);
    let upgraded = br#"{"title":"Session","version":2}"#.to_vec();
    assert_eq!(state.read_current(&FORMAT).unwrap(), Some(upgraded.clone()));
    assert_eq!(on_disk(&path, kind), (upgraded, false));
    let mode = std::fs::metadata(path.join(kind.file()))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // Renaming `name` to `title` adds one byte, so only a record below the limit still fits.
    assert!(FORMAT.upgrade(&sized_v1(kind.limit() - 1)).is_some());

    for (case, record) in [
        ("current", br#"{"version":2,"title":"Session"}"#.to_vec()),
        ("future", br#"{"version":3,"title":"Session"}"#.to_vec()),
        (
            "below the floor",
            br#"{"version":0,"name":"Session"}"#.to_vec(),
        ),
        (
            "refused step",
            br#"{"version":1,"label":"Session"}"#.to_vec(),
        ),
        (
            "refused by the owner",
            br#"{"version":1,"name":7}"#.to_vec(),
        ),
        (
            "unknown field",
            br#"{"version":1,"name":"a","extra":1}"#.to_vec(),
        ),
        (
            "duplicate key",
            br#"{"version":1,"name":"a","name":"b"}"#.to_vec(),
        ),
        (
            "version not a number",
            br#"{"version":"1","name":"a"}"#.to_vec(),
        ),
        ("not an object", b"[1]".to_vec()),
        ("malformed", b"{\"version\":1,".to_vec()),
        ("upgrade over the record limit", sized_v1(kind.limit())),
    ] {
        plant(&path, kind, &record);
        assert_eq!(
            state.read_current(&FORMAT).unwrap(),
            Some(record.clone()),
            "{case}"
        );
        assert_eq!(on_disk(&path, kind), (record, false), "{case}");
    }

    // A user-authored kind upgrades only through the migration; the harness still cannot replace it.
    let profiles = synthetic(RecordKind::ProviderProfiles);
    plant(&path, profiles.kind, br#"{"version":1,"name":"local"}"#);
    let upgraded = br#"{"title":"local","version":2}"#.to_vec();
    assert_eq!(
        state.read_current(&profiles).unwrap(),
        Some(upgraded.clone())
    );
    assert_eq!(on_disk(&path, profiles.kind), (upgraded, false));
    assert!(matches!(
        state.record(profiles.kind).replace(b"{}"),
        Err(StoreError::ReadOnly)
    ));
}

#[test]
fn an_interrupted_upgrade_keeps_the_original_and_finishes_on_the_next_read() {
    let temp = tempfile::tempdir().unwrap();
    let (state, path) = root(temp.path());
    let kind = FORMAT.kind;
    let original = br#"{"version":1,"name":"Session"}"#.to_vec();
    let upgraded = br#"{"title":"Session","version":2}"#.to_vec();

    plant(&path, kind, &original);
    FAIL_AT.with(|fail| fail.set(Some(0)));
    assert!(state.read_current(&FORMAT).is_err());
    FAIL_AT.with(|fail| fail.set(None));
    assert_eq!(on_disk(&path, kind), (original.clone(), true));
    assert_eq!(
        std::fs::read(path.join(kind.pending())).unwrap(),
        upgraded,
        "the crash leaves only the pending replacement behind"
    );
    assert_eq!(state.read_current(&FORMAT).unwrap(), Some(upgraded.clone()));
    assert_eq!(on_disk(&path, kind), (upgraded.clone(), false));

    plant(&path, kind, &original);
    FAIL_AT.with(|fail| fail.set(Some(1)));
    assert!(state.read_current(&FORMAT).is_err());
    FAIL_AT.with(|fail| fail.set(None));
    assert_eq!(on_disk(&path, kind), (upgraded.clone(), false));
    assert_eq!(state.read_current(&FORMAT).unwrap(), Some(upgraded));
}

#[test]
fn upgrades_hold_the_record_writer_lock_and_errors_never_carry_record_content() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let (state, path) = root(temp.path());
    let kind = FORMAT.kind;
    let original = format!(r#"{{"version":1,"name":"{CANARY}"}}"#).into_bytes();
    let mut errors = Vec::new();

    plant(&path, kind, &original);
    state
        .with_account_replacement_lock(&workspace, || {
            std::thread::scope(|scope| {
                let other = scope.spawn(|| {
                    StateRoot::open_existing(&path)
                        .unwrap()
                        .read_current(&FORMAT)
                });
                let busy = other.join().unwrap().unwrap_err();
                assert!(matches!(busy, StoreError::AccountBusy));
                errors.push(busy);
            });
            assert_eq!(on_disk(&path, kind), (original.clone(), false));
            assert!(
                state
                    .read_current(&FORMAT)
                    .unwrap()
                    .is_some_and(|record| current(&record)),
                "the thread holding the writer lock upgrades under it"
            );
        })
        .unwrap();

    plant(&path, kind, &original);
    FAIL_AT.with(|fail| fail.set(Some(0)));
    errors.push(state.read_current(&FORMAT).unwrap_err());
    FAIL_AT.with(|fail| fail.set(None));
    std::fs::remove_file(path.join(kind.pending())).unwrap();
    std::fs::set_permissions(
        path.join(kind.file()),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    errors.push(state.read_current(&FORMAT).unwrap_err());
    std::fs::remove_file(path.join(kind.file())).unwrap();
    let target = temp.path().join("target");
    plant(temp.path(), kind, &original);
    std::fs::rename(temp.path().join(kind.file()), &target).unwrap();
    std::os::unix::fs::symlink(&target, path.join(kind.file())).unwrap();
    errors.push(state.read_current(&FORMAT).unwrap_err());

    assert_eq!(errors.len(), 4);
    for error in errors {
        let text = format!("{error} {error:?}");
        assert!(!text.contains(CANARY), "an error carried record content");
    }
    assert_eq!(std::fs::read(&target).unwrap(), original);
}
