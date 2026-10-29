use super::*;
use crate::store::state::build_cache::tests::{identity, private_dir};
mod sync;

fn upper(path: &Path, files: &[(&str, &[u8])]) -> Dir {
    for (name, bytes) in files {
        std::fs::create_dir_all(path.join(name).parent().unwrap()).unwrap();
        std::fs::write(path.join(name), bytes).unwrap();
    }
    Dir::open_ambient_dir(path, cap_std::ambient_authority()).unwrap()
}

#[test]
fn directory_cache_reuses_identity_and_requires_clear_after_interruption() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    private_dir(&workspace);
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    let identity = identity(&workspace);
    let mut lease = state.begin_build_cache(identity.clone()).unwrap();
    let lower = lease.prepare_directory().unwrap();
    assert!(lower.entries().unwrap().next().is_none());
    assert!(matches!(
        state.begin_build_cache(identity.clone()),
        Err(StoreError::BuildCacheBusy)
    ));
    assert!(matches!(
        state.clear_build_cache(identity.clone()),
        Err(StoreError::BuildCacheBusy)
    ));
    let upper = upper(
        &temp.path().join("upper"),
        &[("target/nested/old", b"old"), ("target/nested/new", b"new")],
    );
    let changes = [
        CacheChange::Directory("target".into()),
        CacheChange::Directory("target/nested".into()),
        CacheChange::File("target/nested/old".into()),
        CacheChange::File("target/nested/new".into()),
    ];
    lease
        .publish_directory(&lower, &upper, &changes, &[], 0)
        .unwrap();
    lease
        .publish_directory(
            &lower,
            &upper,
            &[CacheChange::Remove("target/nested/old".into())],
            &[],
            0,
        )
        .unwrap();
    assert_eq!(measure(&lower).unwrap(), 3);
    assert_eq!(state.build_cache_bytes().unwrap(), 3);
    let guard_id = uuid::Uuid::now_v7();
    lease.guard_started(guard_id).unwrap();
    drop(lower);
    drop(lease);
    let state = StateRoot::open_existing(&state_path).unwrap();
    assert!(matches!(
        state.begin_build_cache(identity.clone()),
        Err(StoreError::BuildCacheBusy)
    ));
    assert!(matches!(
        state.clear_build_cache(identity.clone()),
        Err(StoreError::BuildCacheBusy)
    ));
    let recoveries = state.cache_guard_recoveries(Some(&workspace)).unwrap();
    assert_eq!(recoveries.len(), 1);
    assert_eq!(recoveries[0].id, guard_id);
    for recovery in recoveries {
        recovery.complete().unwrap();
    }
    let mut lease = state.begin_build_cache(identity.clone()).unwrap();
    let lower = lease.prepare_directory().unwrap();
    assert!(lower.symlink_metadata("target/nested/old").is_err());
    assert_eq!(lower.read("target/nested/new").unwrap(), b"new");
    crate::fault::FaultPlan::fail_record_sync(identity.token().into(), "target");
    assert!(
        lease
            .publish_directory(
                &lower,
                &upper,
                &[CacheChange::File("target/nested/old".into())],
                &[],
                0,
            )
            .is_err()
    );
    assert!(lease.publication_uncertain());
    drop(lower);
    drop(lease);
    let reopened = StateRoot::open_existing(&state_path).unwrap();
    assert!(matches!(
        reopened.begin_build_cache(identity.clone()),
        Err(StoreError::InvalidBuildCache)
    ));
    reopened.clear_build_cache(identity.clone()).unwrap();
    assert!(
        reopened
            .begin_build_cache(identity)
            .unwrap()
            .prepare_directory()
            .unwrap()
            .entries()
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn set_size_and_disk_reserve_reject_publication_before_mutation() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    private_dir(&workspace);
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let identity = identity(&workspace);
    let mut lease = state
        .begin_build_cache_with_limit(identity.clone(), MEBIBYTE_BYTES)
        .unwrap();
    let lower = lease.prepare_directory().unwrap();
    let small = upper(&temp.path().join("small"), &[("target/kept", b"kept")]);
    lease
        .publish_directory(
            &lower,
            &small,
            &[CacheChange::File("target/kept".into())],
            &[],
            0,
        )
        .unwrap();
    let large = vec![0; MEBIBYTE_BYTES as usize];
    let large = upper(&temp.path().join("large"), &[("target/large", &large)]);
    for (limit, reserve) in [(MEBIBYTE_BYTES, 0), (MEBIBYTE_BYTES * 2, u64::MAX / 2)] {
        lease.candidate_limit = limit;
        assert!(matches!(
            lease.publish_directory(
                &lower,
                &large,
                &[CacheChange::File("target/large".into())],
                &[],
                reserve,
            ),
            Err(StoreError::StorageFull)
        ));
        assert!(!lease.publication_uncertain());
        assert!(lower.symlink_metadata("target/large").is_err());
        assert_eq!(lower.read("target/kept").unwrap(), b"kept");
    }
    drop(lower);
    drop(lease);
    drop(
        state
            .begin_build_cache(identity)
            .unwrap()
            .prepare_directory()
            .unwrap(),
    );
}

#[test]
// Not isolated: it takes one lease once, and 17,000 synced files can outlast the isolated child's
// bound under a loaded test run.
fn directory_cache_publishes_more_entries_than_the_old_archive_cap() {
    const ENTRIES: usize = 17_000;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    private_dir(&workspace);
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let identity = identity(&workspace);
    let upper_path = temp.path().join("upper");
    std::fs::create_dir_all(upper_path.join("target/deps")).unwrap();
    let mut changes = vec![
        CacheChange::Directory("target".into()),
        CacheChange::Directory("target/deps".into()),
    ];
    for index in 0..ENTRIES {
        let path = format!("target/deps/{index}");
        std::fs::write(upper_path.join(&path), b"x").unwrap();
        changes.push(CacheChange::File(path));
    }
    let upper = Dir::open_ambient_dir(&upper_path, cap_std::ambient_authority()).unwrap();
    let mut lease = state.begin_build_cache(identity).unwrap();
    let lower = lease.prepare_directory().unwrap();
    lease
        .publish_directory(&lower, &upper, &changes, &[], 0)
        .unwrap();
    assert_eq!(measure(&lower).unwrap(), ENTRIES as u64);
}

#[test]
fn cargo_hard_links_publish_as_independent_files() {
    use std::os::unix::fs::MetadataExt as _;
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    private_dir(&workspace);
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    let identity = identity(&workspace);
    let upper_path = temp.path().join("upper");
    let upper = upper(&upper_path, &[("target/debug/deps/app-1234", b"binary")]);
    std::fs::hard_link(
        upper_path.join("target/debug/deps/app-1234"),
        upper_path.join("target/debug/app"),
    )
    .unwrap();
    let mut lease = state.begin_build_cache(identity.clone()).unwrap();
    let lower = lease.prepare_directory().unwrap();
    let changes = ["target", "target/debug", "target/debug/deps"]
        .map(|path| CacheChange::Directory(path.into()))
        .into_iter()
        .chain(
            ["target/debug/app", "target/debug/deps/app-1234"]
                .map(|path| CacheChange::File(path.into())),
        )
        .collect::<Vec<_>>();
    lease
        .publish_directory(&lower, &upper, &changes, &[], 0)
        .unwrap();
    let tree = state_path
        .join(DIRECTORY)
        .join(tree_name(&identity.token()));
    let app = std::fs::metadata(tree.join("target/debug/app")).unwrap();
    let deps = std::fs::metadata(tree.join("target/debug/deps/app-1234")).unwrap();
    assert_eq!((app.nlink(), deps.nlink()), (1, 1));
    assert_ne!(app.ino(), deps.ino());
    assert_eq!(lower.read("target/debug/app").unwrap(), b"binary");
}

#[test]
fn headroom_counts_the_committed_cache_and_free_space_above_the_reserve() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    private_dir(&workspace);
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let identity = identity(&workspace);
    let mut lease = state.begin_build_cache(identity.clone()).unwrap();
    let lower = lease.prepare_directory().unwrap();
    let artifact = vec![0; MEBIBYTE_BYTES as usize * 3];
    let upper = upper(
        &temp.path().join("upper"),
        &[("target/artifact", &artifact)],
    );
    lease
        .publish_directory(
            &lower,
            &upper,
            &[CacheChange::File("target/artifact".into())],
            &[],
            0,
        )
        .unwrap();
    drop(lower);
    drop(lease);
    assert_eq!(
        state.build_cache_headroom(&identity, u64::MAX).unwrap(),
        MEBIBYTE_BYTES * 3
    );
    let open = state.build_cache_headroom(&identity, 0).unwrap();
    assert!(open > MEBIBYTE_BYTES * 3);
    assert_eq!(open % MEBIBYTE_BYTES, 0);
    assert_eq!(state.build_cache_bytes().unwrap(), MEBIBYTE_BYTES * 3);
}
