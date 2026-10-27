use super::*;
use crate::store::state::build_cache::tests::{archive, identity, private_dir, publish};
mod sync;

#[test]
fn directory_cache_migration_reuses_identity_without_changing_the_archive() {
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
    let lease = state.begin_build_cache(identity.clone()).unwrap();
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
    let lease = state.begin_build_cache(identity).unwrap();
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
    let lease = state
        .begin_build_cache_with_limits(identity.clone(), MEBIBYTE_BYTES, MEBIBYTE_BYTES)
        .unwrap();
    let lower = lease.prepare_directory().unwrap();
    let reserve = open_regular(&lease.dir, &reservation_name(&lease.token), 96).unwrap();
    assert_eq!(
        validate_reservation(&reserve, &lease.token).unwrap(),
        MEBIBYTE_BYTES * 4
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
    let lease = state
        .begin_build_cache_with_limits(identity, MEBIBYTE_BYTES, MEBIBYTE_BYTES)
        .unwrap();
    assert!(matches!(
        lease.prepare_directory(),
        Err(StoreError::StorageFull)
    ));
}
