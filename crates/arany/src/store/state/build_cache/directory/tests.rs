use super::*;
use crate::store::state::build_cache::tests::{archive, identity, private_dir, publish};
mod sync;

#[test]
fn directory_cache_migrates_archives_reuses_identity_and_requires_clear_after_interruption() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    private_dir(&workspace);
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    let identity = identity(&workspace);
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("target/nested")).unwrap();
    std::fs::write(source.join("target/nested/old"), b"old").unwrap();
    publish(
        &state,
        identity.clone(),
        &archive(&identity, &source, &[]),
        &[],
    );
    let mut lease = state.begin_build_cache(identity.clone()).unwrap();
    lease.dir.create_dir(tree_name(&lease.token)).unwrap();
    let mut stamp = create_private_file(&lease.dir, &stamp_name(&lease.token)).unwrap();
    let generation = lease.generation().unwrap().unwrap();
    stamp
        .write_all(&generation[..GENERATION_BYTES - 1])
        .unwrap();
    stamp.sync_all().unwrap();
    assert!(matches!(
        lease.prepare_directory(),
        Err(StoreError::InvalidBuildCache)
    ));
    stamp.rewind().unwrap();
    stamp.write_all(&generation).unwrap();
    stamp.sync_all().unwrap();
    let lower = lease.prepare_directory().unwrap();
    assert_eq!(lower.read("target/nested/old").unwrap(), b"old");
    assert!(!file_exists(&lease.dir, &archive_name(&lease.token)).unwrap());
    assert!(!file_exists(&lease.dir, &stamp_name(&lease.token)).unwrap());
    assert!(matches!(
        state.begin_build_cache(identity.clone()),
        Err(StoreError::BuildCacheBusy)
    ));
    assert!(matches!(
        state.clear_build_cache(identity.clone()),
        Err(StoreError::BuildCacheBusy)
    ));
    let upper_path = temp.path().join("upper");
    std::fs::create_dir_all(upper_path.join("target/nested")).unwrap();
    std::fs::write(upper_path.join("target/nested/new"), b"new").unwrap();
    let upper = Dir::open_ambient_dir(&upper_path, cap_std::ambient_authority()).unwrap();
    let changes = [
        CacheChange::Remove("target/nested/old".into()),
        CacheChange::File("target/nested/new".into()),
    ];
    lease
        .publish_directory(&lower, &upper, &changes, &[])
        .unwrap();
    assert!(file_exists(&lease.dir, &reservation_name(&lease.token)).unwrap());
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
    drop(lower);
    drop(lease);
    let dir = cache_directory(&state, false).unwrap();
    let committed = inventory(&dir).unwrap();
    let mut reuse = state.begin_build_cache(identity.clone()).unwrap();
    assert_eq!(
        inventory(&dir).unwrap(),
        committed + DEFAULT_ARCHIVE_BYTES * 2
    );
    let lower = reuse.prepare_directory().unwrap();
    assert_eq!(
        inventory(&dir).unwrap(),
        committed + DEFAULT_ARCHIVE_BYTES * 2
    );
    drop(lower);
    drop(reuse);
    let reader = state
        .read_build_cache(identity.clone(), DEFAULT_ARCHIVE_BYTES)
        .unwrap();
    let (mut stored, _) = reader.archive().unwrap().unwrap();
    let restored_path = temp.path().join("restored");
    private_dir(&restored_path);
    let restored = Dir::open_ambient_dir(&restored_path, cap_std::ambient_authority()).unwrap();
    restore_archive(&mut stored, &restored, &identity, &[]).unwrap();
    assert_eq!(restored.read("target/nested/new").unwrap(), b"new");
    assert!(restored.symlink_metadata("target/nested/old").is_err());
    assert!(matches!(
        state.clear_build_cache(identity.clone()),
        Err(StoreError::BuildCacheBusy)
    ));
    drop(reader);
    let mut lease = state.begin_build_cache(identity.clone()).unwrap();
    let lower = lease.prepare_directory().unwrap();
    crate::fault::FaultPlan::fail_record_sync(identity.token().into(), "target");
    assert!(
        lease
            .publish_directory(
                &lower,
                &upper,
                &[CacheChange::File("target/nested/new".into())],
                &[]
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
fn directory_cache_reserves_aggregate_headroom_and_rejects_overflow_before_mutation() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    private_dir(&workspace);
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let identity = identity(&workspace);
    let (view, candidate) = state
        .effective_build_cache_bytes(&identity, MAX_ARCHIVE_BYTES)
        .unwrap();
    assert_eq!(view, candidate);
    assert!(view < MAX_ARCHIVE_BYTES);
    let mut admission = state
        .begin_build_cache_with_limits(identity.clone(), view, candidate)
        .unwrap();
    admission.prepare_directory().unwrap();
    drop(admission);
    let mut lease = state
        .begin_build_cache_with_limits(identity.clone(), MEBIBYTE_BYTES, MEBIBYTE_BYTES)
        .unwrap();
    let lower = lease.prepare_directory().unwrap();
    let reserve = open_regular(&lease.dir, &reservation_name(&lease.token), 96).unwrap();
    assert_eq!(
        validate_reservation(&reserve, &lease.token).unwrap(),
        MEBIBYTE_BYTES * 2
    );
    let upper_path = temp.path().join("upper");
    std::fs::create_dir_all(upper_path.join("target")).unwrap();
    std::fs::File::create(upper_path.join("target/large"))
        .unwrap()
        .set_len(MEBIBYTE_BYTES)
        .unwrap();
    let upper = Dir::open_ambient_dir(upper_path, cap_std::ambient_authority()).unwrap();
    assert!(matches!(
        lease.publish_directory(
            &lower,
            &upper,
            &[CacheChange::File("target/large".into())],
            &[]
        ),
        Err(StoreError::StorageFull)
    ));
    assert!(!lease.publication_uncertain());
    assert!(lower.entries().unwrap().next().is_none());
    directory_available(&lease.dir, &lease.token).unwrap();
    drop(lower);
    drop(lease);
    let other = temp.path().join("other");
    private_dir(&other);
    let other_identity = super::super::tests::identity(&other);
    let dir = cache_directory(&state, false).unwrap();
    create_private_file(&dir, &archive_name(&other_identity.token()))
        .unwrap()
        .set_len(AGGREGATE_BYTES - MEBIBYTE_BYTES * 3)
        .unwrap();
    let mut lease = state
        .begin_build_cache_with_limits(identity, MEBIBYTE_BYTES * 2, MEBIBYTE_BYTES)
        .unwrap();
    assert!(matches!(
        lease.prepare_directory(),
        Err(StoreError::StorageFull)
    ));
}

#[test]
fn directory_cache_migration_reuses_identity_without_changing_contents() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    private_dir(&workspace);
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    let identity = identity(&workspace);
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("target/nested")).unwrap();
    std::fs::write(source.join("target/nested/old"), b"old").unwrap();
    publish(
        &state,
        identity.clone(),
        &archive(&identity, &source, &[]),
        &[],
    );
    let mut lease = state.begin_build_cache(identity.clone()).unwrap();
    let lower = lease.prepare_directory().unwrap();
    assert_eq!(lower.read("target/nested/old").unwrap(), b"old");
    assert!(matches!(
        state.begin_build_cache(identity.clone()),
        Err(StoreError::BuildCacheBusy)
    ));
    assert!(matches!(
        state.clear_build_cache(identity.clone()),
        Err(StoreError::BuildCacheBusy)
    ));
    drop(lower);
    drop(lease);
    let mut lease = state.begin_build_cache(identity).unwrap();
    assert_eq!(
        lease
            .prepare_directory()
            .unwrap()
            .read("target/nested/old")
            .unwrap(),
        b"old"
    );
}

#[test]
fn directory_cache_reserves_aggregate_headroom_before_conversion() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    private_dir(&workspace);
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let identity = identity(&workspace);
    let mut lease = state
        .begin_build_cache_with_limits(identity.clone(), MEBIBYTE_BYTES * 2, MEBIBYTE_BYTES)
        .unwrap();
    let lower = lease.prepare_directory().unwrap();
    let reserve = open_regular(&lease.dir, &reservation_name(&lease.token), 96).unwrap();
    assert_eq!(
        validate_reservation(&reserve, &lease.token).unwrap(),
        MEBIBYTE_BYTES * 3
    );
    drop(lower);
    drop(lease);
    let other = temp.path().join("other");
    private_dir(&other);
    let other_identity = super::super::tests::identity(&other);
    let dir = cache_directory(&state, false).unwrap();
    create_private_file(&dir, &archive_name(&other_identity.token()))
        .unwrap()
        .set_len(AGGREGATE_BYTES - MEBIBYTE_BYTES * 3)
        .unwrap();
    let mut lease = state
        .begin_build_cache_with_limits(identity, MEBIBYTE_BYTES * 2, MEBIBYTE_BYTES)
        .unwrap();
    assert!(matches!(
        lease.prepare_directory(),
        Err(StoreError::StorageFull)
    ));
}
