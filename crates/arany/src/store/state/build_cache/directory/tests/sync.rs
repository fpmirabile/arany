use super::*;

#[test]
fn nested_ancestor_sync_failure_keeps_conversion_and_application_unavailable_on_reopen() {
    if crate::test_process::isolate_test() {
        return;
    }
    for (conversion, ancestor) in [true, false].into_iter().flat_map(|conversion| {
        ["cargo", "cargo/registry", ".npm", ".cache/pip"].map(|ancestor| (conversion, ancestor))
    }) {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        private_dir(&workspace);
        let state_path = temp.path().join("state");
        let state = StateRoot::admit(&state_path).unwrap();
        let identity = identity(&workspace);
        let source = temp.path().join("source");
        let paths = [
            "cargo/registry/cache/entry",
            ".npm/_cacache/entry",
            ".cache/pip/wheels/entry",
        ];
        for path in paths {
            std::fs::create_dir_all(source.join(path).parent().unwrap()).unwrap();
            std::fs::write(source.join(path), b"prior").unwrap();
        }
        publish(
            &state,
            identity.clone(),
            &archive(&identity, &source, &[]),
            &[],
        );
        let mut lease = state.begin_build_cache(identity.clone()).unwrap();
        if conversion {
            crate::fault::FaultPlan::fail_record_sync(identity.token().into(), ancestor);
            assert!(lease.prepare_directory().is_err(), "conversion {ancestor}");
        } else {
            let lower = lease.prepare_directory().unwrap();
            std::fs::write(source.join(paths[0]), b"updated").unwrap();
            let upper = Dir::open_ambient_dir(&source, cap_std::ambient_authority()).unwrap();
            crate::fault::FaultPlan::fail_record_sync(identity.token().into(), ancestor);
            assert!(
                lease
                    .publish_directory(&lower, &upper, &[CacheChange::File(paths[0].into())], &[])
                    .is_err(),
                "application {ancestor}"
            );
            assert!(lease.publication_uncertain());
            assert!(!file_exists(&lease.dir, &stamp_name(&identity.token())).unwrap());
            assert!(!file_exists(&lease.dir, &archive_name(&identity.token())).unwrap());
        }
        drop(lease);
        let state = StateRoot::open_existing(&state_path).unwrap();
        assert!(
            matches!(
                state.begin_build_cache(identity.clone()),
                Err(StoreError::InvalidBuildCache)
            ),
            "reopen {ancestor}"
        );
        state.clear_build_cache(identity.clone()).unwrap();
        assert!(
            state
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
}
