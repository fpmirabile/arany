use super::*;
use std::os::unix::fs::DirBuilderExt;

pub(super) fn private_dir(path: &Path) {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .expect("private fixture directory");
}

pub(super) fn identity(path: &Path) -> BuildCacheIdentity {
    let opened = crate::private_fs::open_workspace_directory(path).expect("Workspace admission");
    let metadata = opened.dir_metadata().expect("Workspace metadata");
    BuildCacheIdentity::new(path, metadata.dev(), metadata.ino()).expect("Workspace cache identity")
}

fn ordinary_dir(path: &Path) {
    std::fs::DirBuilder::new()
        .mode(0o755)
        .create(path)
        .expect("ordinary Workspace directory");
}

// A committed directory cache with one file, last used at `used` seconds after the epoch.
fn committed(dir: &Dir, token: &str, used: i64) {
    let tree = format!("{TREE_PREFIX}{token}.tree");
    dir.create_dir(&tree).expect("cache tree");
    dir.create_dir(format!("{tree}/target"))
        .expect("cache root");
    dir.write(format!("{tree}/target/artifact"), b"built")
        .expect("cache artifact");
    let (lock, _) = open_lock_file(dir, &identity_lock_name(token)).expect("identity lock");
    let at = rustix::fs::Timespec {
        tv_sec: used,
        tv_nsec: 0,
    };
    rustix::fs::futimens(
        &lock,
        &rustix::fs::Timestamps {
            last_access: at,
            last_modification: at,
        },
    )
    .expect("last use");
}

fn tree_exists(dir: &Dir, token: &str) -> bool {
    file_exists(dir, &format!("{TREE_PREFIX}{token}.tree")).expect("tree check")
}

#[test]
fn ordinary_workspace_cache_admission_pins_identity_and_rejects_overlap() {
    if crate::test_process::isolate_test() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("fixture");
    let state_path = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    private_dir(&state_path);
    ordinary_dir(&workspace);
    std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o755))
        .expect("ordinary Workspace mode");
    let state = StateRoot::admit(&state_path).expect("StateRoot");
    let admitted = identity(&workspace);
    drop(
        state
            .begin_build_cache(admitted)
            .expect("ordinary Workspace cache admission"),
    );
    state
        .clear_workspace_build_cache(&workspace)
        .expect("clear ordinary Workspace cache");

    let drifting = temp.path().join("drifting-workspace");
    let displaced = temp.path().join("displaced-workspace");
    ordinary_dir(&drifting);
    let stale_identity = identity(&drifting);
    std::fs::rename(&drifting, &displaced).expect("preserve original inode");
    ordinary_dir(&drifting);
    assert!(matches!(
        state.begin_build_cache(stale_identity),
        Err(StoreError::InvalidStateDirectory)
    ));

    let overlap_workspace = temp.path().join("overlap-workspace");
    ordinary_dir(&overlap_workspace);
    let nested_state_path = overlap_workspace.join("state");
    private_dir(&nested_state_path);
    let nested_state = StateRoot::admit(&nested_state_path).expect("nested StateRoot");
    assert!(matches!(
        nested_state.begin_build_cache(identity(&overlap_workspace)),
        Err(StoreError::StateNotPrivate)
    ));
}

#[test]
fn one_workspace_cache_lease_excludes_a_second_command() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().expect("fixture");
    let state_path = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    let other_workspace = temp.path().join("other-workspace");
    private_dir(&state_path);
    private_dir(&workspace);
    private_dir(&other_workspace);
    let state = StateRoot::admit(&state_path).expect("StateRoot");
    let workspace_identity = identity(&workspace);
    let lease = state
        .begin_build_cache(workspace_identity.clone())
        .expect("first cache lease");
    assert!(matches!(
        state.begin_build_cache(workspace_identity.clone()),
        Err(StoreError::BuildCacheBusy)
    ));
    assert!(state.begin_build_cache(identity(&other_workspace)).is_ok());
    drop(lease);
    assert!(state.begin_build_cache(workspace_identity).is_ok());
}

#[test]
fn eviction_removes_least_recently_used_caches_and_keeps_leased_and_guarded_ones() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().expect("fixture");
    let state = StateRoot::admit(&temp.path().join("state")).expect("StateRoot");
    let dir = cache_directory(&state, true).expect("cache directory");
    let [leased, superseded, guarded, oldest, newer, newest] =
        std::array::from_fn(|index| format!("{index:064x}"));
    committed(&dir, &leased, 10);
    committed(&dir, &superseded, 20);
    dir.write(format!("cache-{superseded}.arc"), b"old archive")
        .expect("superseded archive");
    committed(&dir, &guarded, 30);
    dir.write(format!("{GUARD_PREFIX}{guarded}.unit"), b"unit")
        .expect("Guard record");
    committed(&dir, &oldest, 40);
    committed(&dir, &newer, 50);
    committed(&dir, &newest, 60);
    let held = lock_file(&dir, &identity_lock_name(&leased), true).expect("live lease");
    {
        let _namespace = lock_file(&dir, NAMESPACE_LOCK, true).expect("namespace");
        evict_lru(&dir, |dir| Ok(!tree_exists(dir, &newer))).expect("evict until newer goes");
    }
    for evicted in [&superseded, &oldest] {
        assert!(!tree_exists(&dir, evicted), "{evicted}");
        assert!(!file_exists(&dir, &identity_lock_name(evicted)).unwrap());
    }
    assert!(!file_exists(&dir, &format!("cache-{superseded}.arc")).unwrap());
    assert!(tree_exists(&dir, &newest));
    for kept in [&leased, &guarded] {
        assert!(tree_exists(&dir, kept), "{kept}");
    }

    assert!(state.evict_build_caches(0).expect("nothing needed") > 0);
    assert!(tree_exists(&dir, &newest));
    state
        .evict_build_caches(u64::MAX)
        .expect("evict everything evictable");
    assert!(!tree_exists(&dir, &newest));
    for kept in [&leased, &guarded] {
        assert!(tree_exists(&dir, kept), "{kept}");
    }
    drop(held);
}

#[test]
fn identity_cap_evicts_the_least_recently_used_unleased_cache() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().expect("fixture");
    let state_path = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    private_dir(&state_path);
    private_dir(&workspace);
    let state = StateRoot::admit(&state_path).expect("StateRoot");
    let dir = cache_directory(&state, true).expect("cache directory");
    let tokens = (0..MAX_IDENTITIES)
        .map(|index| format!("{index:064x}"))
        .collect::<Vec<_>>();
    let mut held = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        committed(&dir, token, 100 + index as i64);
        held.push(lock_file(&dir, &identity_lock_name(token), true).expect("live lease"));
    }
    assert!(matches!(
        state.begin_build_cache(identity(&workspace)),
        Err(StoreError::StorageFull)
    ));
    drop(held);
    drop(
        state
            .begin_build_cache(identity(&workspace))
            .expect("admitted after eviction"),
    );
    assert!(!tree_exists(&dir, &tokens[0]));
    assert!(tokens[1..].iter().all(|token| tree_exists(&dir, token)));
    assert_eq!(identity_count(&dir).unwrap(), MAX_IDENTITIES);
}

#[test]
fn superseded_archive_records_are_discarded_at_the_next_lease() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().expect("fixture");
    let workspace = temp.path().join("workspace");
    private_dir(&workspace);
    let state = StateRoot::admit(&temp.path().join("state")).expect("StateRoot");
    let identity = identity(&workspace);
    let dir = cache_directory(&state, true).expect("cache directory");
    let token = identity.token();
    for (prefix, ending) in SUPERSEDED_KINDS {
        let name = format!("{prefix}{token}{ending}");
        dir.write(&name, b"superseded").expect("superseded record");
        assert_eq!(inventory_record(&name), Some((Some(token.as_str()), true)));
        drop(
            state
                .begin_build_cache(identity.clone())
                .expect("a regenerable older-format cache never blocks its Workspace"),
        );
        assert!(!file_exists(&dir, &name).unwrap());
        assert!(file_exists(&dir, &identity_lock_name(&token)).unwrap());
    }
    dir.write(SUPERSEDED_LOCK, b"").expect("superseded lock");
    assert_eq!(inventory_record(SUPERSEDED_LOCK), Some((None, false)));
    state.clear_all_build_caches().expect("clear all");
    assert!(!file_exists(&dir, SUPERSEDED_LOCK).unwrap());
}
