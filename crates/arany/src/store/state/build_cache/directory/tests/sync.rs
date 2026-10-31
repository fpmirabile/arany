use super::*;

#[test]
fn nested_ancestor_sync_failure_keeps_application_unavailable_on_reopen() {
    if crate::test_process::isolate_test() {
        return;
    }
    for ancestor in ["cargo", "cargo/registry", ".npm", ".cache/pip"] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        private_dir(&workspace);
        let state_path = temp.path().join("state");
        let state = StateRoot::admit(&state_path).unwrap();
        let identity = identity(&workspace);
        let paths = [
            "cargo/registry/cache/entry",
            ".npm/_cacache/entry",
            ".cache/pip/wheels/entry",
        ];
        let source = upper(
            &temp.path().join("source"),
            &paths.map(|path| (path, b"prior".as_slice())),
        );
        let mut lease = state.begin_build_cache(identity.clone()).unwrap();
        let lower = lease.prepare_directory().unwrap();
        let mut changes = paths
            .iter()
            .flat_map(|path| {
                Path::new(path)
                    .ancestors()
                    .skip(1)
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map(|parent| parent.to_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .filter(|parent| validate_path(parent).is_ok())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(CacheChange::Directory)
            .collect::<Vec<_>>();
        changes.extend(paths.map(|path| CacheChange::File(path.into())));
        lease
            .publish_directory(&lower, &source, &changes, &[], 0)
            .unwrap();
        for path in paths {
            std::fs::write(temp.path().join("source").join(path), b"updated").unwrap();
        }
        crate::fault::FaultPlan::fail_record_sync(identity.token().into(), ancestor);
        assert!(
            lease
                .publish_directory(
                    &lower,
                    &source,
                    &paths.map(|path| CacheChange::File(path.into())),
                    &[],
                    0
                )
                .is_err(),
            "application {ancestor}"
        );
        assert!(lease.publication_uncertain());
        drop(lower);
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
